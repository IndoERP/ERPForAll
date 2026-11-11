//! Core tables owned by framework crates above the ORM (`erp-rpc`'s
//! `ir_idempotency_key`), installed by [`crate::Orm::sync_schema`] like the signal table: in the
//! schema transaction, idempotently (`CREATE … IF NOT EXISTS`), before the models' tables.
//!
//! A crate registers a table with [`core_ddl!`](crate::core_ddl). Business addon tables
//! come from models (the layer check refuses `erp-orm` as an addon dependency). The native
//! framework `base` addon alone uses a reserved SDK bridge for its hierarchy fence; this
//! does not expose a supported core-table extension API to business addons.

/// One core table's DDL.
#[derive(Debug)]
pub struct CoreDdl {
    /// The owning crate, for error messages.
    pub owner: &'static str,
    /// The entry's name (its main table), for error messages.
    pub name: &'static str,
    /// What the tables are for, for error messages ("the node registry").
    pub purpose: &'static str,
    /// Every table the DDL creates: a model may not use one of them as its table.
    pub tables: &'static [&'static str],
    /// Idempotent DDL (`CREATE TABLE IF NOT EXISTS …; CREATE INDEX IF NOT EXISTS …`).
    pub sql: &'static str,
    /// Idempotent DDL run after the models' tables exist, in the same schema transaction: what
    /// refers to a model's table (a foreign key to `res_users`).
    pub after_models: Option<AfterModels>,
}

/// The part of a core table's DDL that refers to a model's table ([`CoreDdl::after_models`]):
/// run after the models' tables, and only when a model of the registry owns `needs_table` (a
/// registry without `res.users` has no `res_users` to refer to).
#[derive(Debug, Clone, Copy)]
pub struct AfterModels {
    /// The model table the DDL refers to.
    pub needs_table: &'static str,
    /// Idempotent DDL.
    pub sql: &'static str,
}

/// When [`run`] creates core tables: before the models' tables, or after them.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePhase {
    /// [`CoreDdl::sql`] of every core table.
    BeforeModels,
    /// [`CoreDdl::after_models`] of those whose model table the registry has.
    AfterModels,
}

/// Core tables linked into the binary.
#[linkme::distributed_slice]
pub static CORE_DDL: [CoreDdl];

/// Register a core table installed by [`crate::Orm::sync_schema`].
///
/// ```
/// erp_orm::core_ddl!(
///     DEMO_TABLE,
///     owner = "my-crate",
///     name = "erp_demo",
///     purpose = "the demo",
///     tables = ["erp_demo"],
///     sql = "CREATE TABLE IF NOT EXISTS erp_demo (id int8 PRIMARY KEY)"
/// );
///
/// fn main() {
///     assert!(erp_orm::CORE_DDL.iter().any(|t| t.name == "erp_demo"));
/// }
/// ```
#[macro_export]
macro_rules! core_ddl {
    (
        $ident:ident,
        owner = $owner:literal,
        name = $name:literal,
        purpose = $purpose:literal,
        tables = [$($table:literal),+ $(,)?],
        sql = $sql:expr
    ) => {
        #[$crate::linkme::distributed_slice($crate::CORE_DDL)]
        #[linkme(crate = $crate::linkme)]
        static $ident: $crate::CoreDdl = $crate::CoreDdl {
            owner: $owner,
            name: $name,
            purpose: $purpose,
            tables: &[$($table),+],
            sql: $sql,
            after_models: None,
        };
    };
    (
        $ident:ident,
        owner = $owner:literal,
        name = $name:literal,
        purpose = $purpose:literal,
        tables = [$($table:literal),+ $(,)?],
        sql = $sql:expr,
        after_models = { needs_table = $needs:literal, sql = $after:expr }
    ) => {
        #[$crate::linkme::distributed_slice($crate::CORE_DDL)]
        #[linkme(crate = $crate::linkme)]
        static $ident: $crate::CoreDdl = $crate::CoreDdl {
            owner: $owner,
            name: $name,
            purpose: $purpose,
            tables: &[$($table),+],
            sql: $sql,
            after_models: Some($crate::AfterModels {
                needs_table: $needs,
                sql: $after,
            }),
        };
    };
}

/// A core table a binary requires ([`crate::OrmBuilder::require_core_tables`]): the
/// expectation that a linked [`core_ddl!`](crate::core_ddl) registration of `owner` creates
/// `table`. An owner crate's own list (such as `erp_security::auth::core_tables()`) also
/// references the registration, so a binary that holds the list links it: the refusal for a
/// missing registration fires for hand-written lists, as defence in depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreTableRequirement {
    table: &'static str,
    owner: &'static str,
}

impl CoreTableRequirement {
    /// `table`, registered by the crate `owner`.
    pub const fn new(table: &'static str, owner: &'static str) -> Self {
        Self { table, owner }
    }

    /// The table.
    pub const fn table(&self) -> &'static str {
        self.table
    }

    /// The crate that registers it.
    pub const fn owner(&self) -> &'static str {
        self.owner
    }
}

/// Core tables of `erp-orm` itself, created before those of the crates above it.
pub(crate) const ORM_DDL: &[CoreDdl] = &[
    crate::node::NODE_TABLES,
    crate::vacuum::VACUUM_TABLES,
    crate::schema_generated::GENERATED_TABLES,
    crate::jobs::schema::JOB_TABLES,
];

/// Every core table: `erp-orm`'s own, then the registered ones (what [`crate::Orm::sync_schema`]
/// creates, in order).
pub(crate) fn all() -> impl Iterator<Item = &'static CoreDdl> {
    ORM_DDL.iter().chain(CORE_DDL.iter())
}

/// Run the core tables' DDL of `phase` in `tx`, in order: the one sequence
/// [`crate::Orm::sync_schema`] and the test harness share (before the models' tables, then after
/// them). A failure names the table and its owner and keeps the SQLSTATE and the source; the
/// caller ends the transaction.
///
/// # Errors
///
/// The DDL's errors, as above.
#[doc(hidden)]
pub async fn run(tx: &mut crate::Transaction, phase: CorePhase) -> erp_fields::Result<()> {
    for ddl in all() {
        let (sql, doing) = match phase {
            CorePhase::BeforeModels => (ddl.sql, "creating"),
            CorePhase::AfterModels => match ddl.after_models {
                Some(after)
                    if tx
                        .env()
                        .registry()
                        .model_by_table(after.needs_table)
                        .is_some() =>
                {
                    // An upgrade converges the core tables before it creates the tables of new
                    // models (an older database has no `ir_attachment` yet): the schema step of
                    // that upgrade runs this phase again once the table exists.
                    let present = tx
                        .query("SELECT to_regclass($1) IS NOT NULL", &[&after.needs_table])
                        .await?[0]
                        .get::<_, bool>(0);
                    if !present {
                        tracing::debug!(
                            target: "erp::schema",
                            table = ddl.name,
                            owner = ddl.owner,
                            needs = after.needs_table,
                            "core table DDL after the models deferred: the model's table does \
                             not exist yet"
                        );
                        continue;
                    }
                    (after.sql, "completing (after the models)")
                }
                // the registry has no model of the table it refers to: nothing to refer to
                // (documented on `AfterModels`); said at debug so "why no FK" has an answer in
                // the logs (silent-delta N-10)
                Some(after) => {
                    tracing::debug!(
                        target: "erp::schema",
                        table = ddl.name,
                        owner = ddl.owner,
                        needs = after.needs_table,
                        "core table DDL after the models skipped: no model of the registry owns \
                         the table it refers to"
                    );
                    continue;
                }
                None => continue,
            },
        };
        if let Err(error) = tx.batch(sql).await {
            return Err(erp_fields::Error::new(
                error.kind(),
                format!(
                    "{doing} the core table '{}' of '{}' failed: {}",
                    ddl.name,
                    ddl.owner,
                    error.message()
                ),
            )
            .with_sqlstate(error.sqlstate().map(str::to_owned))
            .with_source(error));
        }
    }
    Ok(())
}

/// Check the registered tables: every table of `required` is created by a linked registration,
/// entry names and tables are unique, and no model of `registry` uses a core table as its own
/// (the model sync would treat it as the model's table). Public for the test harness.
///
/// # Errors
///
/// `Programming` naming the missing table and its owner crate, the duplicate, or the model.
#[doc(hidden)]
pub fn validate(
    registry: &erp_registry::Registry,
    required: &[&CoreTableRequirement],
) -> erp_fields::Result<()> {
    for need in required {
        match all().find(|ddl| ddl.tables.contains(&need.table)) {
            None => {
                return Err(erp_fields::Error::programming(format!(
                    "core table '{}' of '{}' is required but not registered in this binary: \
                     nothing linked references the core_ddl! registration of '{}', so the linker \
                     dropped it; reference the owner's list (e.g. \
                     erp_security::auth::core_tables()) from the binary",
                    need.table, need.owner, need.owner
                )));
            }
            Some(ddl) if ddl.owner != need.owner => {
                return Err(erp_fields::Error::programming(format!(
                    "core table '{}' is required as a table of '{}' but is registered by '{}'",
                    need.table, need.owner, ddl.owner
                )));
            }
            Some(_) => {}
        }
    }
    let mut seen: Vec<&CoreDdl> = Vec::new();
    for ddl in all() {
        if let Some(first) = seen.iter().find(|d| d.name == ddl.name) {
            return Err(erp_fields::Error::programming(format!(
                "core table '{}' is registered twice: by '{}' and by '{}'",
                ddl.name, first.owner, ddl.owner
            )));
        }
        for table in ddl.tables {
            if let Some(first) = seen.iter().find(|d| d.tables.contains(table)) {
                return Err(erp_fields::Error::programming(format!(
                    "core table '{table}' is created by '{}' and by '{}'",
                    first.owner, ddl.owner
                )));
            }
        }
        seen.push(ddl);
    }
    for model in registry.models().iter().filter(|m| m.has_table()) {
        if let Some(ddl) = seen
            .iter()
            .find(|d| d.tables.contains(&model.table.as_str()))
        {
            return Err(erp_fields::Error::programming(format!(
                "model {} uses table {}, a core table of {} ({}): give the model another table",
                model.name, model.table, ddl.owner, ddl.purpose
            )));
        }
    }
    Ok(())
}
