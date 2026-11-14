// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon-owned SQL objects in schema sync (slice B3; plan
//! ). Inside the schema transaction, with
//! the schema lock held: S0 plans and refuses before any DDL, S1 installs independent functions
//! before the tables, S2 renders catalog-derived bodies after the column changes, S3 installs
//! triggers after the signal triggers, S4 records provenance. The provenance row, never the
//! catalog looked up by name, is the comparator (D1); DDL runs only when a hash differs (D3)
//! and only in an addon install or upgrade (A6).

use std::collections::{BTreeMap, BTreeSet};
#[path = "schema_sqlobj_integrity.rs"]
mod integrity;
#[path = "schema_sqlobj_uninstall.rs"]
mod uninstall;
pub use uninstall::SqlObjectRemoval;

use erp_fields::{Error, ErrorKind, Result};
use erp_registry::{
    ModelSpec, Registry, SqlCompareColumn, SqlFunction, SqlLanguage, SqlObject, SqlObjectGraph,
    SqlObjectId, SqlObjectKind, SqlReference,
};
use erp_sql::sqlobj;
use sha2::{Digest as _, Sha256};

use crate::Transaction;

/// The SQL object provenance table.
pub(crate) const OBJECT_TABLE: &str = "erp_schema_sql_object";
/// The ordered dependency edges of the provenance rows.
pub(crate) const DEPENDENCY_TABLE: &str = "erp_schema_sql_object_dependency";

/// DDL of the provenance tables (contract §4): owner-only metadata, never a model. Hashes are
/// SHA-256 of the canonical payloads of `erp_sql::sqlobj`, format version 1.
pub(crate) const SQL_OBJECT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_schema_sql_object (
    addon                text   NOT NULL,
    key                  text   NOT NULL,
    kind                 text   NOT NULL CHECK (kind IN ('function', 'trigger', 'condition', 'foreign_key', 'table_options')),
    physical_identity    text   NOT NULL UNIQUE,
    format_version       int4   NOT NULL,
    shape                text   NOT NULL,
    shape_sha256         bytea  NOT NULL,
    declaration_sha256   bytea  NOT NULL,
    body_sha256          bytea,
    catalog_input_sha256 bytea,
    installed_sha256     bytea,
    revision             int8   NOT NULL CHECK (revision > 0),
    step_state           text   NOT NULL,
    PRIMARY KEY (addon, key)
);
CREATE TABLE IF NOT EXISTS erp_schema_sql_object_dependency (
    addon       text NOT NULL,
    key         text NOT NULL,
    position    int4 NOT NULL CHECK (position >= 0),
    target_kind text NOT NULL CHECK (target_kind IN ('object', 'model')),
    target      text NOT NULL,
    PRIMARY KEY (addon, key, position),
    FOREIGN KEY (addon, key) REFERENCES erp_schema_sql_object (addon, key) ON DELETE CASCADE
);";

/// Core DDL entry of the provenance tables (`erp-orm`).
pub(crate) const SQL_OBJECT_TABLES: crate::CoreDdl = crate::CoreDdl {
    owner: "erp-orm",
    name: OBJECT_TABLE,
    purpose: "the SQL object provenance catalog",
    tables: &[OBJECT_TABLE, DEPENDENCY_TABLE],
    sql: SQL_OBJECT_DDL,
    after_models: None,
};

/// The only step state of B3: installed in the committing transaction.
const INSTALLED: &str = "installed";

fn refuse(id: SqlObjectId, reason: impl std::fmt::Display) -> Error {
    Error::programming(format!("SQL object '{id}': {reason}"))
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn unreadable(what: &str, source: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        format!("unreadable SQL object catalog: {what}"),
    )
    .with_source(source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Unchanged,
    Create,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the tables (S1).
    Early,
    /// After the column changes (S2): catalog-derived bodies and what depends on them.
    Derived,
    /// After the signal triggers (S3).
    Trigger,
    /// Composite FKs after tables and unique keys.
    Integrity,
    /// No DDL, provenance only (conditions, B5 maps them).
    Record,
}

/// Only a lifecycle subset may defer new objects until another module creates their table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlObjectScope {
    Full,
    Subset,
}

struct Row {
    installed: Option<Vec<u8>>,
    shape: String,
    declaration: Vec<u8>,
    body: Option<Vec<u8>>,
}

struct Item<'r> {
    object: &'r SqlObject,
    phase: Phase,
    action: Action,
    row: Option<Row>,
    schemas: Vec<String>,
    declaration: Vec<u8>,
    shape: String,
    body: Option<String>,
    catalog_input: Option<Vec<u8>>,
}

impl<'r> Item<'r> {
    fn function(&self) -> Option<&'r SqlFunction> {
        match self.object.kind() {
            SqlObjectKind::Function(f) => Some(f),
            _ => None,
        }
    }
}

/// The SQL object work of one schema sync, planned by S0.
pub(crate) struct SqlObjectSync<'r> {
    items: Vec<Item<'r>>,
    lifecycle: bool,
    create_tables: bool,
    ran: bool,
    upgrade_integrity: bool,
    /// The validated role bindings (B4); `None` when the registry declares no SQL object.
    roles: Option<crate::schema_sqlobj_roles::BoundRoles>,
    graph: &'r SqlObjectGraph,
    deferred: BTreeSet<SqlObjectId>,
}

impl SqlObjectSync<'_> {
    /// Objects omitted by this lifecycle subset must not be admitted or installed yet.
    pub(crate) fn is_deferred(&self, id: SqlObjectId) -> bool {
        self.deferred.contains(&id)
    }

    /// A table recreated by partition sync would discard recorded integrity identities.
    pub(crate) fn refuse_integrity_recreation(&self, tables: &[String]) -> Result<()> {
        for item in &self.items {
            if item.row.is_none()
                || !matches!(
                    item.object.kind(),
                    SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_)
                )
            {
                continue;
            }
            for reference in item.object.references() {
                if let SqlReference::Model(model) = reference
                    && tables.iter().any(|table| table == model.table())
                {
                    return Err(refuse(
                        item.object.id(),
                        format!(
                            "referenced table '{}' is recreated by partition synchronization; explicit migration required",
                            model.table()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A6: refuse the first object that needs a change outside a lifecycle transaction.
    fn admit(&self) -> Result<()> {
        if self.lifecycle {
            return Ok(());
        }
        let first = self.items.iter().find(|i| {
            i.action != Action::Unchanged || self.create_tables || self.upgrade_integrity
        });
        match first {
            Some(item) => Err(refuse(
                item.object.id(),
                "SQL object DDL runs only inside an addon install or upgrade",
            )),
            None => Ok(()),
        }
    }
}

impl Transaction {
    /// S0: read provenance and the catalog, refuse D2, D6, D7, D11, shape changes and drift,
    /// validate the role bindings of `options` (B4), decide per object, refuse A6. No DDL.
    /// `provenance` says whether the provenance table exists; `synced` are the specs this sync
    /// creates or alters.
    pub(crate) async fn plan_sql_objects<'r>(
        &mut self,
        registry: &'r Registry,
        provenance: bool,
        synced: &[&ModelSpec],
        options: &crate::SchemaSyncOptions,
        scope: SqlObjectScope,
    ) -> Result<SqlObjectSync<'r>> {
        let graph = registry.sql_objects();
        let mut sync = SqlObjectSync {
            items: Vec::new(),
            lifecycle: options.is_lifecycle(),
            create_tables: !provenance && !graph.objects().is_empty(),
            ran: false,
            upgrade_integrity: false,
            roles: None,
            graph,
            deferred: BTreeSet::new(),
        };
        if graph.objects().is_empty() && !provenance {
            return Ok(sync);
        }
        let needed = graph.objects().iter().any(|o| {
            matches!(
                o.kind(),
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_)
            )
        });
        sync.upgrade_integrity = self.plan_integrity_catalog(provenance, needed).await?;
        let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
        if provenance {
            let addons: Vec<&str> = registry.addons().iter().map(|a| a.name).collect();
            let found = self
                .query(
                    "SELECT addon, key, format_version, step_state, shape, declaration_sha256, \
                     body_sha256, installed_sha256 FROM erp_schema_sql_object WHERE addon = ANY($1) ORDER BY \
                     addon, key",
                    &[&addons],
                )
                .await?;
            for row in &found {
                let get = |i: usize| -> Result<String> {
                    row.try_get(i).map_err(|e| unreadable("provenance row", e))
                };
                let (addon, key) = (get(0)?, get(1)?);
                let version: i32 = row.try_get(2).map_err(|e| unreadable("format", e))?;
                if version != sqlobj::FORMAT_VERSION || get(3)? != INSTALLED {
                    return Err(Error::new(
                        ErrorKind::Database,
                        format!(
                            "SQL object '{addon}/{key}': provenance format {version} or step \
                             state is not supported by this build"
                        ),
                    ));
                }
                if graph.get(&addon, &key).is_none() {
                    return Err(Error::programming(format!(
                        "SQL object '{addon}/{key}': recorded in SQL object provenance but no \
                         longer declared while addon '{addon}' is installed; removing it needs \
                         a later release"
                    )));
                }
                rows.insert(
                    (addon, key),
                    Row {
                        installed: row
                            .try_get(7)
                            .map_err(|e| unreadable("installed hash", e))?,
                        shape: get(4)?,
                        declaration: row.try_get(5).map_err(|e| unreadable("hash", e))?,
                        body: row.try_get(6).map_err(|e| unreadable("hash", e))?,
                    },
                );
            }
        }
        if graph.objects().is_empty() {
            return Ok(sync);
        }

        // the current schema (A1 for model dependencies) and which referenced tables exist now
        let current: Option<String> = self
            .query("SELECT current_schema()::text", &[])
            .await?
            .first()
            .map(|r| r.try_get(0))
            .transpose()
            .map_err(|e| unreadable("current schema", e))?
            .flatten();
        let current = current.ok_or_else(|| {
            Error::programming("SQL objects need a current schema; the search_path names none")
        })?;
        let mut referenced: BTreeSet<&str> = BTreeSet::new();
        for object in graph.objects() {
            for reference in object.references() {
                if let SqlReference::Model(m) = reference {
                    referenced.insert(m.table());
                }
            }
        }
        let names: Vec<&str> = referenced.iter().copied().collect();
        let quoted: Vec<String> = names.iter().map(|t| erp_sql::quote(t)).collect();
        let exist = self
            .flags(
                "SELECT to_regclass(u.q) IS NOT NULL FROM unnest($1::text[]) WITH ORDINALITY AS \
                 u(q, i) ORDER BY u.i",
                &quoted,
                None,
            )
            .await?;
        let existing: BTreeSet<&str> = names
            .iter()
            .zip(exist)
            .filter(|(_, e)| *e)
            .map(|(t, _)| *t)
            .collect();
        let synced: BTreeSet<&str> = synced.iter().map(|s| s.table.as_str()).collect();

        // catalog presence of each function and trigger identity
        let mut signatures = Vec::new();
        let mut trigger_tables = Vec::new();
        let mut trigger_names = Vec::new();
        for object in graph.objects() {
            match object.kind() {
                SqlObjectKind::Function(f) => signatures.push(sqlobj::signature(f)),
                SqlObjectKind::Trigger(t) => {
                    trigger_tables.push(erp_sql::quote(t.model().table()));
                    trigger_names.push(t.name().to_owned());
                }
                _ => {}
            }
        }
        let functions = self
            .flags(
                "SELECT to_regprocedure(u.s) IS NOT NULL FROM unnest($1::text[]) WITH ORDINALITY \
                 AS u(s, i) ORDER BY u.i",
                &signatures,
                None,
            )
            .await?;
        let triggers = self
            .flags(
                "SELECT EXISTS (SELECT 1 FROM pg_trigger t WHERE t.tgrelid = to_regclass(u.q) \
                 AND t.tgname = u.n) FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS \
                 u(q, n, i) ORDER BY u.i",
                &trigger_tables,
                Some(&trigger_names),
            )
            .await?;
        let (mut functions, mut triggers) = (functions.into_iter(), triggers.into_iter());

        let mut derived: BTreeSet<SqlObjectId> = BTreeSet::new();
        for object in graph.objects() {
            let id = object.id();
            // Read every identity before deferral so a missing dependency never hides a collision.
            let observed = match object.kind() {
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_) => {
                    integrity::observed(self, object).await?
                }
                _ => None,
            };
            // Consume every positional catalog flag before any deferral/skip.
            let present = match object.kind() {
                SqlObjectKind::Function(_) => functions.next().ok_or_else(missing_flag)?,
                SqlObjectKind::Trigger(_) => triggers.next().ok_or_else(missing_flag)?,
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_) => observed.is_some(),
                _ => false,
            };
            let row = rows.remove(&(id.addon().to_owned(), id.key().to_owned()));
            let missing = object.references().iter().find_map(|reference| match reference {
                SqlReference::Model(m) if !existing.contains(m.table()) && !synced.contains(m.table()) =>
                    Some(format!("table '{}' of model '{}' does not exist and is not created by this sync", m.table(), m.model())),
                SqlReference::Object(dep) if sync.deferred.contains(dep) =>
                    Some(format!("dependency '{dep}' is deferred until a later schema step")),
                _ => None,
            });
            if let Some(reason) = missing {
                if scope != SqlObjectScope::Subset || !sync.lifecycle || row.is_some() {
                    return Err(refuse(id, reason));
                }
                if present {
                    return Err(refuse(
                        id,
                        format!(
                            "{} exists but is not owned by SQL object provenance; adopting it needs an explicit migration",
                            sqlobj::physical_identity(object)
                        ),
                    ));
                }
                sync.deferred.insert(id);
                continue;
            }
            let shape = sqlobj::shape_payload(object);
            if let Some(row) = &row
                && let Some(label) = sqlobj::shape_change(object, &row.shape)?
            {
                return Err(refuse(
                    id,
                    format!("{label} changed; explicit migration required"),
                ));
            }
            let schemas = sqlobj::search_path_schemas(graph, object, &current);
            let (phase, present, category) = match object.kind() {
                SqlObjectKind::Function(f) => {
                    let late = f.column_set().is_some()
                        || object.references().iter().any(
                            |r| matches!(r, SqlReference::Object(dep) if derived.contains(dep)),
                        );
                    let phase = if late {
                        derived.insert(id);
                        Phase::Derived
                    } else {
                        Phase::Early
                    };
                    if phase == Phase::Early && f.language() == SqlLanguage::Sql {
                        for reference in object.references() {
                            if let SqlReference::Model(m) = reference
                                && !existing.contains(m.table())
                            {
                                return Err(refuse(
                                    id,
                                    format!(
                                        "SQL-language function depends on table '{}' created by \
                                         this sync",
                                        m.table()
                                    ),
                                ));
                            }
                        }
                    }
                    (phase, present, "function")
                }
                SqlObjectKind::Trigger(_) => (Phase::Trigger, present, "trigger"),
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_) => {
                    if let Some(row) = &row
                        && observed.as_ref().map(|v| sha256(v.as_bytes())) != row.installed
                    {
                        return Err(refuse(
                            id,
                            "installed integrity differs; reconcile through addon upgrade",
                        ));
                    }
                    let phase = if matches!(object.kind(), SqlObjectKind::ForeignKey(_)) {
                        Phase::Integrity
                    } else {
                        Phase::Record
                    };
                    (phase, observed.is_some(), "integrity")
                }
                SqlObjectKind::Condition(_) => (Phase::Record, false, "condition"),
                // no DDL and no provenance row: reconciled by the grant phase of every lifecycle
                // sync (B4)
                SqlObjectKind::TableGrant(_) => continue,
                _ => return Err(refuse(id, "its kind is installed by a later SQLOBJ slice")),
            };
            // a catalog-derived body is rendered in S2; its declaration covers the renderer
            let body = match object.kind() {
                SqlObjectKind::Function(f) if f.column_set().is_none() => Some(f.body()),
                _ => None,
            };
            let payload = sqlobj::declaration_payload(object, body.unwrap_or(""), &schemas);
            let declaration = sha256(payload.as_bytes());
            let action = match (&row, present) {
                (Some(_), false) if phase != Phase::Record => {
                    return Err(refuse(
                        id,
                        format!("installed {category} differs; reconcile through addon upgrade"),
                    ));
                }
                (Some(row), _) if row.declaration != declaration => Action::Replace,
                (Some(_), _) => Action::Unchanged,
                (None, true) => {
                    return Err(refuse(
                        id,
                        format!(
                            "{} exists but is not owned by SQL object provenance; adopting it \
                             needs an explicit migration",
                            sqlobj::physical_identity(object)
                        ),
                    ));
                }
                (None, false) => Action::Create,
            };
            sync.items.push(Item {
                object,
                phase,
                action,
                row,
                schemas,
                declaration,
                shape,
                body: body.map(str::to_owned),
                catalog_input: None,
            });
        }
        // B6: the same safety admission for model sync and init_sql_objects. Only newly
        // added FKs over existing tables may need a scan; unchanged guards never trigger it.
        let mut candidates: Vec<String> = sync
            .items
            .iter()
            .filter_map(|item| match item.object.kind() {
                SqlObjectKind::ForeignKey(f)
                    if item.action == Action::Create && existing.contains(f.model().table()) =>
                {
                    Some(f.model().table().to_owned())
                }
                _ => None, // only new FKs need validation; every other kind has its own phase
            })
            .collect();
        candidates.sort();
        candidates.dedup();
        let large = self.large_tables(&candidates, options.large_rows()).await?;
        for item in &sync.items {
            if let SqlObjectKind::ForeignKey(f) = item.object.kind()
                && item.action == Action::Create
                && large.contains(f.model().table())
            {
                return Err(refuse(
                    item.object.id(),
                    "deferred integrity installation requires in a later release",
                ));
            }
        }
        sync.admit()?;
        // B4: the bindings are validated before any DDL, over the tables that exist now
        let mut schemas: Vec<String> = vec![current.clone()];
        for object in graph.objects() {
            for schema in &sqlobj::search_path_schemas(graph, object, &current) {
                if !schemas.contains(schema) {
                    schemas.push(schema.clone());
                }
            }
        }
        let existing: Vec<&str> = existing.into_iter().collect();
        let roles = self
            .check_sql_roles(graph, options.sql_role_bindings(), &existing, schemas)
            .await?;
        sync.roles = Some(roles);
        Ok(sync)
    }

    /// One boolean per element of `first` (zipped with `second`), in order.
    async fn flags(
        &mut self,
        sql: &str,
        first: &[String],
        second: Option<&Vec<String>>,
    ) -> Result<Vec<bool>> {
        if first.is_empty() {
            return Ok(Vec::new());
        }
        let found = match second {
            Some(second) => self.query(sql, &[&first, second]).await?,
            None => self.query(sql, &[&first]).await?,
        };
        let flags: Vec<bool> = found
            .iter()
            .map(|r| r.try_get(0).map_err(|e| unreadable("catalog flag", e)))
            .collect::<Result<_>>()?;
        if flags.len() != first.len() {
            return Err(missing_flag());
        }
        Ok(flags)
    }

    /// S1: the provenance tables when missing, then the independent functions.
    pub(crate) async fn install_sql_objects_early(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
    ) -> Result<()> {
        if sync.create_tables {
            self.batch(SQL_OBJECT_DDL).await?;
            sync.ran = true;
        }
        self.install_integrity_catalog(sync).await?;
        self.install_functions(sync, Phase::Early).await
    }

    /// S2: render each catalog-derived body from the final columns of its table and install
    /// it (and what depends on it) when its declaration or rendered body changed.
    pub(crate) async fn install_sql_objects_derived(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
    ) -> Result<()> {
        let mut columns: BTreeMap<String, Vec<SqlCompareColumn>> = BTreeMap::new();
        for at in 0..sync.items.len() {
            let item = &sync.items[at];
            let Some(set) = item
                .function()
                .and_then(|f| f.column_set().map(|c| (f, *c)))
            else {
                continue;
            };
            let (f, set) = set;
            let table = item
                .object
                .references()
                .iter()
                .find_map(|r| match r {
                    SqlReference::Model(m) if m.model() == set.model() => Some(m.table()),
                    _ => None,
                })
                .ok_or_else(|| refuse(item.object.id(), "its compared table is not resolved"))?
                .to_owned();
            if !columns.contains_key(&table) {
                let read = self.compare_columns(&table).await?;
                columns.insert(table.clone(), read);
            }
            let Some(all) = columns.get(&table) else {
                return Err(refuse(
                    item.object.id(),
                    "its compared columns were not read",
                ));
            };
            let compared: Vec<SqlCompareColumn> = all
                .iter()
                .filter(|c| !set.allow().contains(&c.name()))
                .cloned()
                .collect();
            let body = f.render(&compared);
            let id = item.object.id();
            let bound = self.registry().sql_objects().limits().body_bytes;
            if body.len() > bound {
                return Err(refuse(id, format!("body bytes limit {bound} exceeded")));
            }
            let changed_body = item
                .row
                .as_ref()
                .is_some_and(|r| r.body.as_deref() != Some(sha256(body.as_bytes()).as_slice()));
            let item = &mut sync.items[at];
            if item.action == Action::Unchanged && changed_body {
                item.action = Action::Replace;
            }
            item.catalog_input = Some(sha256(sqlobj::columns_payload(&compared).as_bytes()));
            item.body = Some(body);
        }
        sync.admit()?;
        self.install_functions(sync, Phase::Derived).await
    }

    /// The ordinary user columns of `table` in `attnum` order: generated and dropped columns
    /// excluded.
    async fn compare_columns(&mut self, table: &str) -> Result<Vec<SqlCompareColumn>> {
        let found = self
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) FROM pg_attribute \
                 a WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped \
                 AND a.attgenerated = '' ORDER BY a.attnum",
                &[&erp_sql::quote(table)],
            )
            .await?;
        // every model table has at least its id column: none found means the table did not
        // resolve, and an empty list would make the guard compare nothing (review r1 S-6)
        if found.is_empty() {
            return Err(Error::programming(format!(
                "catalog-derived SQL object body: table '{table}' has no comparable column \
                 (missing table)"
            )));
        }
        found
            .iter()
            .map(|r| {
                let name: String = r.try_get(0).map_err(|e| unreadable("column", e))?;
                let ty: String = r.try_get(1).map_err(|e| unreadable("column type", e))?;
                Ok(SqlCompareColumn::new(name, ty))
            })
            .collect()
    }

    async fn install_functions(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
        phase: Phase,
    ) -> Result<()> {
        for item in sync.items.iter().filter(|i| i.phase == phase) {
            let (Some(f), Some(body)) = (item.function(), item.body.as_deref()) else {
                continue;
            };
            let replace = match item.action {
                Action::Unchanged => continue,
                Action::Create => false,
                Action::Replace => true,
            };
            let mut sql = sqlobj::create_function(f, body, &item.schemas, replace)?;
            if !replace {
                // B4: in the creating batch, so PUBLIC never holds EXECUTE on it
                sql = format!("{sql};\n{}", sqlobj::revoke_public(f));
            }
            self.sql_object_ddl(item.object.id(), &sql).await?;
            sync.ran = true;
        }
        Ok(())
    }

    /// S3: create the missing triggers, each `ENABLE ALWAYS`.
    pub(crate) async fn install_sql_objects_triggers(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
    ) -> Result<()> {
        let graph = self.registry().clone();
        let graph = graph.sql_objects();
        for item in sync.items.iter().filter(|i| i.action == Action::Create) {
            let SqlObjectKind::Trigger(t) = item.object.kind() else {
                continue;
            };
            let function = t.function();
            let Some(SqlObjectKind::Function(f)) = graph
                .get(function.addon(), function.key())
                .map(SqlObject::kind)
            else {
                return Err(refuse(
                    item.object.id(),
                    format!("trigger function '{function}' is not resolved"),
                ));
            };
            let sql = format!(
                "{};\n{}",
                sqlobj::create_trigger(t, f),
                sqlobj::enable_always(t)
            );
            self.sql_object_ddl(item.object.id(), &sql).await?;
            sync.ran = true;
        }
        // B4: ownership and grants, only in a lifecycle sync (A6; `admit` refused any other one
        // that would change an object)
        if let (true, Some(roles)) = (sync.lifecycle, &sync.roles) {
            self.grant_sql_objects(sync.graph, roles, &sync.deferred, true)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn sql_object_ddl(&mut self, id: SqlObjectId, sql: &str) -> Result<()> {
        self.batch(sql).await.map_err(|e| {
            Error::new(
                e.kind(),
                format!("SQL object '{id}': installing failed: {}", e.message()),
            )
            .with_sqlstate(e.sqlstate().map(str::to_owned))
            .with_source(e)
        })
    }

    /// S4: write the provenance of every created or changed object, with the installed
    /// definitions read back; signal the schema change when DDL ran.
    pub(crate) async fn record_sql_objects(&mut self, sync: &SqlObjectSync<'_>) -> Result<()> {
        let changed: Vec<&Item<'_>> = sync
            .items
            .iter()
            .filter(|i| i.action != Action::Unchanged)
            .collect();
        if changed.is_empty() {
            if sync.ran {
                self.signal_schema_change().await?;
            }
            return Ok(());
        }
        let mut cols: [Vec<String>; 5] = Default::default();
        let mut hashes: [Vec<Option<Vec<u8>>>; 5] = Default::default();
        let mut edges: [Vec<String>; 4] = Default::default();
        let mut positions: Vec<i32> = Vec::new();
        for item in &changed {
            let id = item.object.id();
            let (kind, installed) = match item.object.kind() {
                SqlObjectKind::Function(f) => {
                    let sql = "SELECT pg_get_functiondef(to_regprocedure($1))";
                    let installed = self.installed(id, sql, &[&sqlobj::signature(f)]).await?;
                    ("function", Some(installed))
                }
                SqlObjectKind::Trigger(t) => {
                    let sql = "SELECT pg_get_triggerdef(t.oid) || ' enabled=' || \
                               t.tgenabled::text FROM pg_trigger t WHERE t.tgrelid = \
                               to_regclass($1) AND t.tgname = $2";
                    let table = erp_sql::quote(t.model().table());
                    let installed = self.installed(id, sql, &[&table, &t.name()]).await?;
                    ("trigger", Some(installed))
                }
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_) => {
                    let observed =
                        integrity::observed(self, item.object)
                            .await?
                            .ok_or_else(|| {
                                refuse(id, "installed integrity is missing after its DDL")
                            })?;
                    let kind = if matches!(item.object.kind(), SqlObjectKind::ForeignKey(_)) {
                        "foreign_key"
                    } else {
                        "table_options"
                    };
                    (kind, Some(sha256(observed.as_bytes())))
                }
                SqlObjectKind::Condition(_) => ("condition", None),
                _ => return Err(refuse(id, "a later object kind cannot be recorded")),
            };
            cols[0].push(id.addon().to_owned());
            cols[1].push(id.key().to_owned());
            cols[2].push(kind.to_owned());
            cols[3].push(sqlobj::physical_identity(item.object));
            cols[4].push(item.shape.clone());
            hashes[0].push(Some(sha256(item.shape.as_bytes())));
            hashes[1].push(Some(item.declaration.clone()));
            hashes[2].push(item.body.as_ref().map(|b| sha256(b.as_bytes())));
            hashes[3].push(item.catalog_input.clone());
            hashes[4].push(installed);
            for (position, reference) in item.object.references().iter().enumerate() {
                let (target_kind, target) = match reference {
                    SqlReference::Object(dep) => ("object", dep.to_string()),
                    SqlReference::Model(m) => ("model", m.model().to_owned()),
                    _ => {
                        return Err(refuse(
                            id,
                            "a reference of a later slice cannot be recorded",
                        ));
                    }
                };
                edges[0].push(id.addon().to_owned());
                edges[1].push(id.key().to_owned());
                edges[2].push(target_kind.to_owned());
                edges[3].push(target);
                positions
                    .push(i32::try_from(position).map_err(|_| refuse(id, "too many references"))?);
            }
        }
        self.execute(
            "INSERT INTO erp_schema_sql_object (addon, key, kind, physical_identity, \
             format_version, shape, shape_sha256, declaration_sha256, body_sha256, \
             catalog_input_sha256, installed_sha256, revision, step_state) SELECT u.a, u.k, \
             u.kind, u.p, $12, u.s, u.sh, u.d, u.b, u.c, u.i, 1, $11 FROM unnest($1::text[], \
             $2::text[], $3::text[], $4::text[], $5::text[], $6::bytea[], $7::bytea[], \
             $8::bytea[], $9::bytea[], $10::bytea[]) AS u(a, k, kind, p, s, sh, d, b, c, i) \
             ON CONFLICT (addon, key) DO UPDATE SET kind = EXCLUDED.kind, physical_identity = \
             EXCLUDED.physical_identity, format_version = EXCLUDED.format_version, shape = \
             EXCLUDED.shape, shape_sha256 = EXCLUDED.shape_sha256, declaration_sha256 = \
             EXCLUDED.declaration_sha256, body_sha256 = EXCLUDED.body_sha256, \
             catalog_input_sha256 = EXCLUDED.catalog_input_sha256, installed_sha256 = \
             EXCLUDED.installed_sha256, revision = erp_schema_sql_object.revision + 1, \
             step_state = EXCLUDED.step_state",
            &[
                &cols[0],
                &cols[1],
                &cols[2],
                &cols[3],
                &cols[4],
                &hashes[0],
                &hashes[1],
                &hashes[2],
                &hashes[3],
                &hashes[4],
                &INSTALLED,
                &sqlobj::FORMAT_VERSION,
            ],
        )
        .await?;
        self.execute(
            "DELETE FROM erp_schema_sql_object_dependency d USING unnest($1::text[], $2::text[]) \
             AS u(a, k) WHERE d.addon = u.a AND d.key = u.k",
            &[&cols[0], &cols[1]],
        )
        .await?;
        if !positions.is_empty() {
            self.execute(
                "INSERT INTO erp_schema_sql_object_dependency (addon, key, position, \
                 target_kind, target) SELECT * FROM unnest($1::text[], $2::text[], $3::int4[], \
                 $4::text[], $5::text[])",
                &[&edges[0], &edges[1], &positions, &edges[2], &edges[3]],
            )
            .await?;
        }
        if sync.ran {
            self.signal_schema_change().await?;
        }
        Ok(())
    }

    /// SHA-256 of the installed definition `sql` reads back (one row, one text column).
    async fn installed(
        &mut self,
        id: SqlObjectId,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<u8>> {
        let found = self.query(sql, params).await?;
        let def: Option<String> = found
            .first()
            .map(|r| r.try_get(0))
            .transpose()
            .map_err(|e| unreadable("installed definition", e))?
            .flatten();
        let def = def.ok_or_else(|| refuse(id, "installed definition is missing after its DDL"))?;
        Ok(sha256(def.as_bytes()))
    }

    /// Install or update the addon-owned SQL objects of the registry alone, for an addon that
    /// declares SQL objects but no table model (an addon schema step with no model to sync):
    /// the same planning, phases, refusals and provenance as
    /// [`Transaction::init_models_with`], which runs them with every model sync. Takes the
    /// schema lock and bounds its waits like it.
    ///
    /// # Errors
    ///
    /// `Programming` for the refusals (an object outside a lifecycle sync (A6), a
    /// shape change, an unowned object of the same identity, a role binding an object names but
    /// the options leave unset, a table that does not exist, an object recorded but no longer
    /// declared, drift); `Access` for an unsafe role binding or a privilege a bound role or
    /// PUBLIC holds beyond the declaration (A2); the DDL's errors, naming the object.
    pub async fn init_sql_objects(&mut self, options: &crate::SchemaSyncOptions) -> Result<()> {
        self.sync_sql_objects(options, SqlObjectScope::Full).await
    }

    /// Reconcile objects of one addon schema step that creates no model table.
    /// New objects of a later module may wait only when `options` marks a lifecycle operation.
    /// The module executor must call [`Self::refuse_deferred_sql_objects`] before commit.
    ///
    /// ```no_run
    /// # async fn step(tx: &mut erp_orm::Transaction, options: &erp_orm::SchemaSyncOptions) -> erp_fields::Result<()> {
    /// tx.init_sql_objects_subset(options).await?;
    /// // Run any remaining module schema steps in this transaction before the final check.
    /// tx.refuse_deferred_sql_objects(options).await
    /// # }
    /// ```
    ///
    /// # Errors
    /// The refusals and database errors of [`Self::init_sql_objects`].
    #[doc(hidden)]
    pub async fn init_sql_objects_subset(
        &mut self,
        options: &crate::SchemaSyncOptions,
    ) -> Result<()> {
        self.sync_sql_objects(options, SqlObjectScope::Subset).await
    }

    /// Refuse incomplete SQL-object work at the end of an addon lifecycle operation.
    /// Reads the final registry, provenance and catalog under the schema lock; never installs
    /// or repairs anything. Policies have no provenance row and are checked by target/grants.
    ///
    /// # Errors
    /// `Programming` names an object no schema step installed or a declaration left stale;
    /// provenance, catalog, role and privilege refusals are as [`Self::init_sql_objects`].
    #[doc(hidden)]
    pub async fn refuse_deferred_sql_objects(
        &mut self,
        options: &crate::SchemaSyncOptions,
    ) -> Result<()> {
        let registry = std::sync::Arc::clone(self.registry());
        self.verify_sql_object_publication(&registry, options).await
    }

    /// Verify the complete installed graph and effective privileges without repairing objects.
    /// The owner-side caller holds lifecycle exclusion and supplies its installed registry.
    /// # Errors
    /// Returns named programming errors for absent/stale objects and Access for unsafe roles or grants.
    pub async fn verify_sql_object_publication(
        &mut self,
        registry: &Registry,
        options: &crate::SchemaSyncOptions,
    ) -> Result<()> {
        self.sql_object_publication(registry, options, false).await
    }

    /// Reconcile declared ACLs only; never creates/replaces an object or transfers ownership.
    /// # Errors
    /// Missing, drifted or re-owned objects require an addon upgrade; unsafe grants refuse and the
    /// caller must roll back this transaction. Takes the existing schema lock.
    pub async fn reconcile_sql_object_grants(
        &mut self,
        registry: &Registry,
        options: &crate::SchemaSyncOptions,
    ) -> Result<()> {
        self.sql_object_publication(registry, options, true).await
    }

    async fn sql_object_publication(
        &mut self,
        registry: &Registry,
        options: &crate::SchemaSyncOptions,
        reconcile: bool,
    ) -> Result<()> {
        let graph = registry.sql_objects();
        let (saved, lock_timeout) = self.set_schema_limits(false).await?;
        self.take_schema_lock(&lock_timeout).await?;
        let present = self.sql_object_provenance_exists().await?;
        let recorded: Vec<_> = graph
            .objects()
            .iter()
            .filter(|o| !matches!(o.kind(), SqlObjectKind::TableGrant(_)))
            .collect();
        let addons: Vec<String> = recorded.iter().map(|o| o.owner().to_owned()).collect();
        let keys: Vec<String> = recorded.iter().map(|o| o.id().key().to_owned()).collect();
        let found = if present {
            self.flags(
                "SELECT EXISTS (SELECT 1 FROM erp_schema_sql_object o WHERE o.addon=u.a AND \
                 o.key=u.k) FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS u(a,k,i) \
                 ORDER BY u.i",
                &addons,
                Some(&keys),
            )
            .await?
        } else {
            vec![false; recorded.len()]
        };
        if let Some((object, _)) = recorded.iter().zip(found).find(|(_, found)| !found) {
            return Err(refuse(
                object.id(),
                "deferred installation was not completed by any schema step of this operation",
            ));
        }
        // Strict planning performs no DDL and preserves all installed-shape/drift refusals.
        let sync = self
            .plan_sql_objects(registry, present, &[], options, SqlObjectScope::Full)
            .await?;
        if let Some(item) = sync.items.iter().find(|i| i.action != Action::Unchanged) {
            return Err(refuse(
                item.object.id(),
                "declaration was not reconciled by any schema step of this operation",
            ));
        }
        if let Some(roles) = &sync.roles {
            if reconcile {
                self.check_sql_object_owners(graph, roles, &BTreeSet::new())
                    .await?;
                self.grant_sql_objects(graph, roles, &BTreeSet::new(), false)
                    .await?;
            } else {
                self.verify_sql_object_privileges(graph, roles, &BTreeSet::new())
                    .await?;
            }
        }
        self.set_timeouts(&saved.0, &saved.1).await
    }

    async fn sql_object_provenance_exists(&mut self) -> Result<bool> {
        self.query("SELECT to_regclass($1) IS NOT NULL", &[&OBJECT_TABLE])
            .await?
            .first()
            .ok_or_else(missing_flag)?
            .try_get(0)
            .map_err(|e| unreadable("provenance table", e))
    }

    async fn sync_sql_objects(
        &mut self,
        options: &crate::SchemaSyncOptions,
        scope: SqlObjectScope,
    ) -> Result<()> {
        let registry = std::sync::Arc::clone(self.registry());
        let (saved, lock_timeout) = self.set_schema_limits(false).await?;
        self.take_schema_lock(&lock_timeout).await?;
        let present = self.sql_object_provenance_exists().await?;
        let mut sync = self
            .plan_sql_objects(&registry, present, &[], options, scope)
            .await?;
        self.install_sql_objects_early(&mut sync).await?;
        self.install_sql_objects_derived(&mut sync).await?;
        self.install_sql_objects_triggers(&mut sync).await?;
        self.install_sql_object_constraints(&mut sync).await?;
        self.record_sql_objects(&sync).await?;
        self.set_timeouts(&saved.0, &saved.1).await
    }
}

fn missing_flag() -> Error {
    Error::new(
        ErrorKind::Database,
        "unreadable SQL object catalog: the catalog answered fewer rows than asked",
    )
}
