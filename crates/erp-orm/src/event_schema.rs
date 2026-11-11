//! Schema DDL and partitions of the outbox event table in the dedicated `erp_event` schema (
//! A2-D5, plan S3). The audit table `ir_audit_entry` is a second
//! [`TableDef`] of the same shape, installed by [`Transaction::install_audit_schema`] with the same
//! checks and a 31-day partition window; everything below that says "the table" holds for either, and
//! [`Transaction::grant_event_runtime_role`] grants every installed one.
//!
//! The table is not created by generic schema sync (which skips the event namespace, see
//! [`erp_registry::ModelSpec::has_generic_table`]): [`Transaction::install_event_schema`] creates it,
//! on the owner connection, from the fixed definition below, and checks an existing one against the
//! same definition instead of adopting it. The install grants nothing and builds no envelope (S5): the
//! runtime role gets its rights from [`Transaction::grant_event_runtime_role`] (see below).
//!
//! What is checked, for the table and every object around it, each time the install runs:
//! - the server is PostgreSQL 18 or later with the built-in `uuidv7()`, `pg_current_xact_id()` and
//!   `transaction_timestamp()` ([`EventSchemaProblem::Admission`]);
//! - schema, table, sequence and partitions are owned by the role that runs the install;
//! - the table is a range-partitioned table by `create_date`, not a view, foreign or plain table;
//! - the columns, their types, `NOT NULL` and the generated defaults are exactly the definition: a
//!   default has to be the built-in function (read with `search_path = pg_catalog`, so a lookalike in
//!   another schema prints qualified and differs), never an identity or generated column;
//! - the primary key `(id, create_date)`, the two indexes, no other constraint, trigger or rule;
//! - the id sequence is a plain `bigint` sequence, owned by the `id` column;
//! - every partition is a plain table of the table, named for one UTC day, with exactly that day as
//!   its range; no default partition (it would forbid `DETACH PARTITION ... CONCURRENTLY` and catch
//!   rows the missing-partition error must reject).
//!
//! Partitions are daily in UTC and the window is today plus [`EVENT_PREMAKE_DAYS`] days. Names and
//! bounds are computed in SQL from `now() AT TIME ZONE 'UTC'` and written with an explicit `+00`
//! offset, and the checks read the bounds with `TimeZone = UTC`, so none of it depends on the
//! session's time zone. There is no default partition: an insert for a day without one fails with
//! PostgreSQL's "no partition of relation found for row".
//!
//! # Grants
//!
//! [`Transaction::grant_event_runtime_role`] gives the runtime role `USAGE` on the schema, `INSERT` on
//! the columns a writer supplies (the columns without a database default, so the generated `id`,
//! `event_id`, `create_date` and `xid` are refused with `42501`), and `USAGE` on the id sequence.
//! Nothing else: no `SELECT`, `UPDATE`, `DELETE`, `TRUNCATE`, no partition or schema DDL, no `setval`,
//! and no right on any partition (an insert through the parent needs the parent's right only, so a
//! partition made later by the upkeep needs no grant). It first revokes everything the role holds on
//! the schema, so a repeat resets what was added by hand. Rights of `PUBLIC`, of roles the role is a
//! member of and default ACLs are not touched here; they are caught by the publication check
//! (`MISSING_RUNTIME_GRANTS_SQL` in `erp-cli`), which reads the effective rights as the role itself.

use std::fmt;

use erp_fields::{Error, Result};

use crate::Transaction;

/// The schema of the closed event namespace.
pub(crate) const SCHEMA: &str = erp_registry::EVENT_SCHEMA;
/// Days ahead of today that the install and the upkeep keep outbox partitions for (
/// decision 5).
pub const EVENT_PREMAKE_DAYS: u32 = 7;

/// One column of the table: what the catalog must show for it.
pub(crate) struct Column {
    pub(crate) name: &'static str,
    /// `format_type` of the column.
    pub(crate) ty: &'static str,
    pub(crate) not_null: bool,
    /// The default as `pg_get_expr` prints it with `search_path = pg_catalog`.
    pub(crate) default: Option<&'static str>,
    /// The default as DDL writes it.
    pub(crate) default_ddl: Option<&'static str>,
}

pub(crate) const fn column(name: &'static str, ty: &'static str, not_null: bool) -> Column {
    Column {
        name,
        ty,
        not_null,
        default: None,
        default_ddl: None,
    }
}

pub(crate) const fn generated(
    name: &'static str,
    ty: &'static str,
    default: &'static str,
    default_ddl: &'static str,
) -> Column {
    Column {
        name,
        ty,
        not_null: true,
        default: Some(default),
        default_ddl: Some(default_ddl),
    }
}

/// The columns, in order. The four generated ones are database defaults (`id` from the sequence,
/// `event_id` uuidv7, `create_date` the transaction's start, `xid` the transaction's xid8); the rest
/// is the envelope, supplied by the capture insert (S5/S6).
pub(crate) const OUTBOX_COLUMNS: &[Column] = &[
    generated(
        "id",
        "bigint",
        "nextval('erp_event.ir_event_outbox_id_seq'::regclass)",
        "pg_catalog.nextval('\"erp_event\".\"ir_event_outbox_id_seq\"'::pg_catalog.regclass)",
    ),
    generated("event_id", "uuid", "uuidv7()", "pg_catalog.uuidv7()"),
    generated(
        "create_date",
        "timestamp with time zone",
        "transaction_timestamp()",
        "pg_catalog.transaction_timestamp()",
    ),
    generated(
        "xid",
        "xid8",
        "pg_current_xact_id()",
        "pg_catalog.pg_current_xact_id()",
    ),
    column("schema_version", "smallint", true),
    column("kind", "text", true),
    column("model", "text", true),
    column("record_ids", "jsonb", true),
    column("field_names", "text[]", true),
    column("cascade_models", "text[]", true),
    column("subject", "text", true),
    column("principal_kind", "text", true),
    column("actor_kind", "text", true),
    column("actor_id", "bigint", true),
    column("delegating_user_id", "bigint", false),
    column("company_ids", "bigint[]", true),
    column("correlation_id", "uuid", false),
    column("operation_id", "uuid", true),
    column("tx_ordinal", "bigint", true),
    column("causation_id", "uuid", false),
];

/// One event table of the closed namespace, as the installer and the checks see it. The outbox
/// ([`OUTBOX`]) and the audit trail ([`crate::audit::AUDIT_TABLE`]) are two definitions of one
/// shape: a range-partitioned table by `create_date` with a generated `id`, a generated unique
/// external id, a cursor index, a BRIN time index and daily UTC partitions.
pub(crate) struct TableDef {
    pub(crate) table: &'static str,
    pub(crate) sequence: &'static str,
    pub(crate) primary_key: &'static str,
    /// Name and column of the unique `(column, create_date)` key.
    pub(crate) key_name: &'static str,
    pub(crate) key_column: &'static str,
    /// The names of every index, for the relkind check.
    pub(crate) index_names: &'static [&'static str],
    /// The indexes as `pg_get_indexdef` prints them with `search_path = pg_catalog`, sorted.
    pub(crate) indexes: &'static [&'static str],
    /// `CREATE INDEX` statements run after the table, beyond the cursor and time indexes.
    pub(crate) extra_index_ddl: &'static [&'static str],
    /// CHECK constraints: name, DDL body, and `pg_get_constraintdef` text.
    pub(crate) checks: &'static [Check],
    pub(crate) columns: &'static [Column],
    pub(crate) premake_days: u32,
}

/// A CHECK constraint of a table definition.
pub(crate) struct Check {
    pub(crate) name: &'static str,
    /// The expression as DDL writes it, without `CHECK`.
    pub(crate) ddl: &'static str,
    /// What `pg_get_constraintdef` prints with `search_path = pg_catalog`.
    pub(crate) printed: &'static str,
}

impl TableDef {
    fn cursor_index(&self) -> String {
        format!("{}_cursor_idx", self.table)
    }
    fn time_index(&self) -> String {
        format!("{}_time_brin", self.table)
    }
    fn qualified(&self) -> String {
        format!("\"{SCHEMA}\".\"{}\"", self.table)
    }
}

pub(crate) const OUTBOX: TableDef = TableDef {
    table: "ir_event_outbox",
    sequence: "ir_event_outbox_id_seq",
    primary_key: "ir_event_outbox_pkey",
    key_name: "ir_event_outbox_event_key",
    key_column: "event_id",
    index_names: &[
        "ir_event_outbox_pkey",
        "ir_event_outbox_event_key",
        "ir_event_outbox_cursor_idx",
        "ir_event_outbox_time_brin",
    ],
    indexes: &[
        "CREATE INDEX ir_event_outbox_cursor_idx ON ONLY erp_event.ir_event_outbox USING btree (xid, id)",
        "CREATE INDEX ir_event_outbox_time_brin ON ONLY erp_event.ir_event_outbox USING brin (create_date)",
        "CREATE UNIQUE INDEX ir_event_outbox_event_key ON ONLY erp_event.ir_event_outbox USING btree (event_id, create_date)",
        "CREATE UNIQUE INDEX ir_event_outbox_pkey ON ONLY erp_event.ir_event_outbox USING btree (id, create_date)",
    ],
    extra_index_ddl: &[],
    checks: &[],
    columns: OUTBOX_COLUMNS,
    premake_days: EVENT_PREMAKE_DAYS,
};

/// Why the install refused an existing object or the server. Reachable as the source of the
/// `Programming` error the install returns (`error.source()` and `downcast_ref`), and named in its
/// message as `(<code>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventSchemaProblem {
    /// The server lacks what the table needs (version, built-in functions, privilege to create).
    Admission(String),
    /// A view, foreign table, plain table or other relation where the table, sequence or index
    /// belongs (`code`: `forged-relation`).
    ForgedRelation(String),
    /// An object owned by a role other than the one running the install (`forged-owner`).
    ForgedOwner(String),
    /// A missing or extra column (`forged-column`).
    ForgedColumn(String),
    /// A column of another type (`forged-type`).
    ForgedType(String),
    /// A column whose `NOT NULL` differs (`forged-nullability`).
    ForgedNullability(String),
    /// A default that is not the built-in generator, or an identity or generated column
    /// (`forged-default`).
    ForgedDefault(String),
    /// A constraint, trigger or rule beyond the primary/UUID composite keys (`forged-constraint`).
    ForgedConstraint(String),
    /// A different primary key or index set (`forged-index`).
    ForgedIndex(String),
    /// A sequence that is not the plain `bigint` sequence owned by `id` (`forged-sequence`).
    ForgedSequence(String),
    /// A default partition (`default-partition`).
    DefaultPartition(String),
    /// A partition that is not a plain table named for one UTC day with exactly that range
    /// (`forged-partition`).
    ForgedPartition(String),
}

impl EventSchemaProblem {
    /// The stable name of the problem, as it appears in the error message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Admission(_) => "admission",
            Self::ForgedRelation(_) => "forged-relation",
            Self::ForgedOwner(_) => "forged-owner",
            Self::ForgedColumn(_) => "forged-column",
            Self::ForgedType(_) => "forged-type",
            Self::ForgedNullability(_) => "forged-nullability",
            Self::ForgedDefault(_) => "forged-default",
            Self::ForgedConstraint(_) => "forged-constraint",
            Self::ForgedIndex(_) => "forged-index",
            Self::ForgedSequence(_) => "forged-sequence",
            Self::DefaultPartition(_) => "default-partition",
            Self::ForgedPartition(_) => "forged-partition",
        }
    }

    /// What is wrong, naming the object.
    pub fn detail(&self) -> &str {
        match self {
            Self::Admission(d)
            | Self::ForgedRelation(d)
            | Self::ForgedOwner(d)
            | Self::ForgedColumn(d)
            | Self::ForgedType(d)
            | Self::ForgedNullability(d)
            | Self::ForgedDefault(d)
            | Self::ForgedConstraint(d)
            | Self::ForgedIndex(d)
            | Self::ForgedSequence(d)
            | Self::DefaultPartition(d)
            | Self::ForgedPartition(d) => d,
        }
    }
}

impl fmt::Display for EventSchemaProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a later release event schema refused ({}): {}; nothing was changed for it, fix or drop the \
             object by hand as the owner role (the install never adopts or repairs it)",
            self.code(),
            self.detail()
        )
    }
}

impl std::error::Error for EventSchemaProblem {}

impl From<EventSchemaProblem> for Error {
    fn from(problem: EventSchemaProblem) -> Self {
        Error::programming(problem.to_string()).with_source(problem)
    }
}

/// What [`Transaction::install_event_schema`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSchemaReport {
    /// The sequence and table were created by this call (and the schema, if it was missing).
    pub created_table: bool,
    /// Partitions created by this call, in date order.
    pub created_partitions: Vec<String>,
    /// Partitions of the window that already existed (and passed the checks), in date order.
    pub existing_partitions: Vec<String>,
}

/// What the server offers, as read by the admission query.
struct AdmissionFacts {
    version_num: i32,
    uuidv7_returns: Option<String>,
    xact_id_returns: Option<String>,
    transaction_timestamp_returns: Option<String>,
    xid8_exists: bool,
    schema_exists: bool,
    can_create_schema: bool,
}

/// PostgreSQL 18 is the first release with `uuidv7()`.
const MIN_SERVER_VERSION_NUM: i32 = 180_000;

fn check_admission(facts: &AdmissionFacts) -> std::result::Result<(), EventSchemaProblem> {
    let refuse = |what: String| Err(EventSchemaProblem::Admission(what));
    if facts.version_num < MIN_SERVER_VERSION_NUM {
        return refuse(format!(
            "the server is PostgreSQL {} but the event table needs 18 or later (built-in uuidv7(); \
             there is no client-side fallback)",
            facts.version_num
        ));
    }
    for (function, found, expected) in [
        ("uuidv7()", &facts.uuidv7_returns, "uuid"),
        ("pg_current_xact_id()", &facts.xact_id_returns, "xid8"),
        (
            "transaction_timestamp()",
            &facts.transaction_timestamp_returns,
            "timestamp with time zone",
        ),
    ] {
        match found.as_deref() {
            Some(ty) if ty == expected => {}
            Some(ty) => {
                return refuse(format!(
                    "pg_catalog.{function} returns {ty}, expected {expected}"
                ));
            }
            None => return refuse(format!("pg_catalog.{function} does not exist")),
        }
    }
    if !facts.xid8_exists {
        return refuse("type pg_catalog.xid8 does not exist".to_owned());
    }
    if !facts.schema_exists && !facts.can_create_schema {
        return refuse(format!(
            "schema \"{SCHEMA}\" does not exist and this role may not create it (run the install \
             as the owner role)"
        ));
    }
    Ok(())
}

const EVENT_GRANT_OWNER: fn(&str, &str) -> String = |role, table| {
    format!(
        "role {role:?} owns {table}: the runtime role must be another role (revoking from \
         the owner would remove what installation and upkeep need)"
    )
};

const EVENT_GRANT_MISSING: fn(&str) -> String = |table| {
    format!(
        "the event table {table} is missing: run Transaction::install_event_schema or \
         Transaction::install_audit_schema before granting the runtime role"
    )
};

/// The runtime role's grant when only the outbox is installed (the checks are shared,
/// `crate::grantee`).
const EVENT_GRANT_OUTBOX: crate::grantee::Grant = crate::grantee::Grant {
    call: "Transaction::grant_event_runtime_role",
    tables: &["erp_event.ir_event_outbox"],
    grantee: "the runtime role",
    missing: EVENT_GRANT_MISSING,
    owner: EVENT_GRANT_OWNER,
};
/// As [`EVENT_GRANT_OUTBOX`] when only the audit table is installed.
const EVENT_GRANT_AUDIT: crate::grantee::Grant = crate::grantee::Grant {
    call: "Transaction::grant_event_runtime_role",
    tables: &["erp_event.ir_audit_entry"],
    grantee: "the runtime role",
    missing: EVENT_GRANT_MISSING,
    owner: EVENT_GRANT_OWNER,
};
/// As [`EVENT_GRANT_OUTBOX`] when both tables are installed.
const EVENT_GRANT_BOTH: crate::grantee::Grant = crate::grantee::Grant {
    call: "Transaction::grant_event_runtime_role",
    tables: &["erp_event.ir_event_outbox", "erp_event.ir_audit_entry"],
    grantee: "the runtime role",
    missing: EVENT_GRANT_MISSING,
    owner: EVENT_GRANT_OWNER,
};

/// One partition of the window, as the SQL computed it.
struct Slot {
    name: String,
    from: String,
    to: String,
}

impl Transaction {
    /// Install the outbox event table: the schema `erp_event`, the id sequence, the partitioned
    /// table with its indexes and today's partition plus the next [`EVENT_PREMAKE_DAYS`] days.
    /// Idempotent: an existing install is checked against the definition (see the module
    /// documentation) and only missing partitions are added. Runs on the owner connection, in this
    /// transaction (a failure leaves nothing behind once the transaction rolls back), serialized
    /// by an advisory lock so two installs cannot interleave. Grants are not part of it.
    ///
    /// # Errors
    ///
    /// `Access` unless this transaction has the System principal with full elevation, checked
    /// before any SQL. `Programming` with an [`EventSchemaProblem`] as its source for anything
    /// that is not exactly the definition; `Database` for a failed statement.
    pub async fn install_event_schema(&mut self) -> Result<EventSchemaReport> {
        self.require_system_full("Transaction::install_event_schema")?;
        self.install_event_table(&OUTBOX).await
    }

    /// Install the audit table `erp_event.ir_audit_entry` the same way as
    /// [`install_event_schema`](Self::install_event_schema) installs the outbox:
    /// today plus [`crate::audit::AUDIT_PREMAKE_DAYS`] daily partitions. Idempotent; an existing
    /// table is checked, never adopted.
    ///
    /// # Errors
    ///
    /// As [`install_event_schema`](Self::install_event_schema).
    pub async fn install_audit_schema(&mut self) -> Result<EventSchemaReport> {
        self.require_system_full("Transaction::install_audit_schema")?;
        self.install_event_table(&crate::audit::AUDIT_TABLE).await
    }

    async fn install_event_table(&mut self, def: &TableDef) -> Result<EventSchemaReport> {
        self.event_lock().await?;
        self.admit_event_schema().await?;
        let created_table = self.create_event_table_if_missing(def).await?;
        self.check_event_table(def).await?;
        let (created, existing) = self.make_event_partitions(def).await?;
        self.check_event_partitions(def).await?;
        Ok(EventSchemaReport {
            created_table,
            created_partitions: created,
            existing_partitions: existing,
        })
    }

    /// Give the runtime role its rights on the event schema: `USAGE` on the schema, `INSERT` on the
    /// columns without a database default of every installed event table (the outbox and the
    /// audit trail), `USAGE` on their id sequences, and nothing else (see the module
    /// documentation). Everything the role held on the schema, its tables (partitions
    /// included) and sequences is revoked first, so the call is idempotent and resets rights added
    /// by hand. With one table installed only that one is granted. The grantee is checked as for
    /// the other core grants
    /// ([`grant_signal_request_role`](Self::grant_signal_request_role)): `PUBLIC`, `pg_*` roles, the
    /// table owner, superusers, roles that reach an owner or a data role are refused.
    ///
    /// # Errors
    ///
    /// `Access` for any principal but the system user; `Programming` for a refused or missing role
    /// and when no event table is installed ([`install_event_schema`](Self::install_event_schema)
    /// first); `Database` when a statement fails (this role may not grant).
    pub async fn grant_event_runtime_role(&mut self, role: &str) -> Result<()> {
        // before any SQL, as `check_grantee` would after the table probe below
        self.require_system_full("Transaction::grant_event_runtime_role")?;
        let installed = self
            .query(
                "SELECT to_regclass('\"erp_event\".\"ir_event_outbox\"') IS NOT NULL, \
                 to_regclass('\"erp_event\".\"ir_audit_entry\"') IS NOT NULL",
                &[],
            )
            .await?;
        let (outbox, audit): (bool, bool) = (installed[0].get(0), installed[0].get(1));
        let (grant, defs): (&crate::grantee::Grant, Vec<&TableDef>) = match (outbox, audit) {
            (true, true) => (&EVENT_GRANT_BOTH, vec![&OUTBOX, &crate::audit::AUDIT_TABLE]),
            (true, false) => (&EVENT_GRANT_OUTBOX, vec![&OUTBOX]),
            (false, true) => (&EVENT_GRANT_AUDIT, vec![&crate::audit::AUDIT_TABLE]),
            (false, false) => (&EVENT_GRANT_OUTBOX, vec![&OUTBOX]),
        };
        let role = self.check_grantee(grant, role).await?;
        let mut sql = format!(
            "REVOKE ALL ON SCHEMA \"{SCHEMA}\" FROM {role};\n\
             REVOKE ALL ON ALL TABLES IN SCHEMA \"{SCHEMA}\" FROM {role};\n\
             REVOKE ALL ON ALL SEQUENCES IN SCHEMA \"{SCHEMA}\" FROM {role};\n\
             GRANT USAGE ON SCHEMA \"{SCHEMA}\" TO {role}"
        );
        for def in defs {
            let supplied: Vec<String> = def
                .columns
                .iter()
                .filter(|c| c.default.is_none())
                .map(|c| format!("\"{}\"", c.name))
                .collect();
            sql.push_str(&format!(
                ";\nGRANT INSERT ({columns}) ON {table} TO {role};\n\
                 GRANT USAGE ON SEQUENCE \"{SCHEMA}\".\"{sequence}\" TO {role}",
                columns = supplied.join(", "),
                table = def.qualified(),
                sequence = def.sequence
            ));
        }
        self.batch(&sql).await
    }

    /// Create the missing partitions of the window (today and [`EVENT_PREMAKE_DAYS`] days after, UTC)
    /// of an installed table, returning the created names in date order. The same checks as the
    /// install run first, so it never adds a partition to a table of the wrong shape.
    ///
    /// # Errors
    ///
    /// As [`Transaction::install_event_schema`]; `Programming` too when the table is not installed.
    pub async fn ensure_event_partitions(&mut self) -> Result<Vec<String>> {
        self.require_system_full("Transaction::ensure_event_partitions")?;
        self.ensure_table_partitions(&OUTBOX, "ensure_event_partitions", "install_event_schema")
            .await
    }

    /// [`ensure_event_partitions`](Self::ensure_event_partitions) for the audit table: today and
    /// [`crate::audit::AUDIT_PREMAKE_DAYS`] days after.
    ///
    /// # Errors
    ///
    /// As [`Transaction::install_audit_schema`]; `Programming` too when the table is not installed.
    pub async fn ensure_audit_partitions(&mut self) -> Result<Vec<String>> {
        self.require_system_full("Transaction::ensure_audit_partitions")?;
        self.ensure_table_partitions(
            &crate::audit::AUDIT_TABLE,
            "ensure_audit_partitions",
            "install_audit_schema",
        )
        .await
    }

    async fn ensure_table_partitions(
        &mut self,
        def: &TableDef,
        call: &str,
        install: &str,
    ) -> Result<Vec<String>> {
        self.event_lock().await?;
        let present = self
            .query("SELECT to_regclass($1) IS NOT NULL", &[&def.qualified()])
            .await?[0]
            .get::<_, bool>(0);
        if !present {
            return Err(Error::programming(format!(
                "{call}: {} is not installed; run {install} first",
                def.qualified()
            )));
        }
        self.check_event_table(def).await?;
        let (created, _) = self.make_event_partitions(def).await?;
        self.check_event_partitions(def).await?;
        Ok(created)
    }

    /// One lock for the whole event namespace: both tables share the schema creation.
    async fn event_lock(&mut self) -> Result<()> {
        self.query(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&format!("{SCHEMA}.ir_event_outbox")],
        )
        .await?;
        Ok(())
    }

    async fn admit_event_schema(&mut self) -> Result<()> {
        let rows = self
            .query(
                "SELECT current_setting('server_version_num')::int, \
                 (SELECT prorettype::regtype::text FROM pg_proc \
                    WHERE oid = to_regprocedure('pg_catalog.uuidv7()')), \
                 (SELECT prorettype::regtype::text FROM pg_proc \
                    WHERE oid = to_regprocedure('pg_catalog.pg_current_xact_id()')), \
                 (SELECT prorettype::regtype::text FROM pg_proc \
                    WHERE oid = to_regprocedure('pg_catalog.transaction_timestamp()')), \
                 to_regtype('pg_catalog.xid8') IS NOT NULL, \
                 EXISTS (SELECT FROM pg_namespace WHERE nspname = $1), \
                 has_database_privilege(current_user, current_database(), 'CREATE')",
                &[&SCHEMA],
            )
            .await?;
        let row = &rows[0];
        let facts = AdmissionFacts {
            version_num: row.get(0),
            uuidv7_returns: row.get(1),
            xact_id_returns: row.get(2),
            transaction_timestamp_returns: row.get(3),
            xid8_exists: row.get(4),
            schema_exists: row.get(5),
            can_create_schema: row.get(6),
        };
        check_admission(&facts)?;
        Ok(())
    }

    /// Check what already sits under our names, create what is missing. Returns whether the table
    /// was created.
    async fn create_event_table_if_missing(&mut self, def: &TableDef) -> Result<bool> {
        let schema = self
            .query(
                "SELECT pg_get_userbyid(nspowner)::text = current_user::text \
                 FROM pg_namespace WHERE nspname = $1",
                &[&SCHEMA],
            )
            .await?;
        match schema.first().map(|r| r.get::<_, bool>(0)) {
            Some(true) => {}
            Some(false) => {
                return Err(EventSchemaProblem::ForgedOwner(format!(
                    "schema \"{SCHEMA}\" is not owned by the installing role"
                ))
                .into());
            }
            None => self.batch(&format!("CREATE SCHEMA \"{SCHEMA}\"")).await?,
        }
        // what sits under our names must be the right kind of relation before we create or trust
        let found = self
            .query(
                "SELECT c.relname::text, c.relkind::text FROM pg_class c \
                 WHERE c.relnamespace = $1::text::regnamespace AND c.relname = ANY($2)",
                &[&format!("\"{SCHEMA}\""), &{
                    let mut names = vec![def.table, def.sequence];
                    names.extend_from_slice(def.index_names);
                    names
                }],
            )
            .await?;
        let mut table_present = false;
        let mut sequence_present = false;
        for row in &found {
            let (name, kind): (String, String) = (row.get(0), row.get(1));
            let expected: &[&str] = if name == def.table {
                &["p"]
            } else if name == def.sequence {
                &["S"]
            } else {
                &["i", "I"]
            };
            if !expected.contains(&kind.as_str()) {
                return Err(EventSchemaProblem::ForgedRelation(format!(
                    "\"{SCHEMA}\".\"{name}\" is a {} (relkind {kind}), expected {}",
                    relkind_name(&kind),
                    if name == def.table {
                        "a partitioned table"
                    } else if name == def.sequence {
                        "a sequence"
                    } else {
                        "an index"
                    }
                ))
                .into());
            }
            table_present |= name == def.table;
            sequence_present |= name == def.sequence;
        }
        if table_present {
            return Ok(false);
        }
        if sequence_present {
            return Err(EventSchemaProblem::ForgedSequence(format!(
                "sequence \"{SCHEMA}\".\"{}\" exists without its table; drop it by hand",
                def.sequence
            ))
            .into());
        }
        self.batch(&create_ddl(def)).await?;
        Ok(true)
    }

    /// Run `check` with `search_path = pg_catalog` and `TimeZone = UTC` for this transaction, then
    /// restore both. Catalog text (default expressions, index definitions, partition bounds) is
    /// printed relative to those two settings, so the comparison would otherwise depend on them.
    async fn with_catalog_view<T>(
        &mut self,
        check: impl AsyncFnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let before = self
            .query(
                "SELECT current_setting('search_path'), current_setting('TimeZone')",
                &[],
            )
            .await?;
        let (path, zone): (String, String) = (before[0].get(0), before[0].get(1));
        self.query(
            "SELECT set_config('search_path', 'pg_catalog', true), \
             set_config('TimeZone', 'UTC', true)",
            &[],
        )
        .await?;
        let result = check(self).await;
        // The settings are transaction-local. After a SQL failure they cannot
        // be restored until rollback; another query would hide the first cause.
        if result.is_err() && self.is_aborted_or_released() {
            return result;
        }
        let restored = self
            .query(
                "SELECT set_config('search_path', $1, true), set_config('TimeZone', $2, true)",
                &[&path, &zone],
            )
            .await;
        match (result, restored) {
            (Err(error), Err(restore)) => Err(error.and_also(&restore)),
            (Err(error), Ok(_)) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(_)) => Ok(value),
        }
    }

    /// Everything about the table except its partitions.
    async fn check_event_table(&mut self, def: &TableDef) -> Result<()> {
        self.with_catalog_view(async |tx| {
            tx.check_event_columns(def).await?;
            tx.check_event_constraints(def).await?;
            tx.check_event_indexes(def).await?;
            tx.check_event_sequence(def).await
        })
        .await
    }

    async fn check_event_columns(&mut self, def: &TableDef) -> Result<()> {
        let qualified = def.qualified();
        let owner = self
            .query(
                "SELECT pg_get_userbyid(relowner)::text = current_user::text, \
                 relkind::text FROM pg_class WHERE oid = $1::text::regclass",
                &[&qualified],
            )
            .await?;
        if !owner[0].get::<_, bool>(0) {
            return Err(EventSchemaProblem::ForgedOwner(format!(
                "table {qualified} is not owned by the installing role"
            ))
            .into());
        }
        let key = self
            .query(
                "SELECT pg_get_partkeydef($1::text::regclass)",
                &[&qualified],
            )
            .await?;
        let key: Option<String> = key[0].get(0);
        if key.as_deref() != Some("RANGE (create_date)") {
            return Err(EventSchemaProblem::ForgedRelation(format!(
                "table {qualified} is partitioned by {}, expected RANGE (create_date)",
                key.as_deref().unwrap_or("nothing")
            ))
            .into());
        }
        let rows = self
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                 pg_get_expr(d.adbin, d.adrelid), a.attgenerated::text, a.attidentity::text \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid \
                 AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&qualified],
            )
            .await?;
        let found: Vec<(String, String, bool, Option<String>, String, String)> = rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
            .collect();
        for (name, ..) in &found {
            if !def.columns.iter().any(|c| c.name == name) {
                return Err(EventSchemaProblem::ForgedColumn(format!(
                    "{qualified} has an unexpected column \"{name}\""
                ))
                .into());
            }
        }
        for want in def.columns {
            let Some((_, ty, not_null, default, generated, identity)) =
                found.iter().find(|(n, ..)| n == want.name)
            else {
                return Err(EventSchemaProblem::ForgedColumn(format!(
                    "{qualified} lacks column \"{}\"",
                    want.name
                ))
                .into());
            };
            if ty != want.ty {
                return Err(EventSchemaProblem::ForgedType(format!(
                    "column \"{}\" of {qualified} has type {ty}, expected {}",
                    want.name, want.ty
                ))
                .into());
            }
            if *not_null != want.not_null {
                return Err(EventSchemaProblem::ForgedNullability(format!(
                    "column \"{}\" of {qualified} is {}, expected {}",
                    want.name,
                    nullability(*not_null),
                    nullability(want.not_null)
                ))
                .into());
            }
            if !generated.is_empty() || !identity.is_empty() {
                return Err(EventSchemaProblem::ForgedDefault(format!(
                    "column \"{}\" of {qualified} is an identity or generated column, expected a \
                     plain column with a default",
                    want.name
                ))
                .into());
            }
            if default.as_deref() != want.default {
                return Err(EventSchemaProblem::ForgedDefault(format!(
                    "column \"{}\" of {qualified} has default {}, expected {}",
                    want.name,
                    default.as_deref().unwrap_or("none"),
                    want.default.unwrap_or("none")
                ))
                .into());
            }
        }
        Ok(())
    }

    async fn check_event_constraints(&mut self, def: &TableDef) -> Result<()> {
        let qualified = def.qualified();
        let constraints = self
            .query(
                "SELECT conname::text, contype::text, pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE conrelid = $1::text::regclass AND contype <> 'n' ORDER BY conname",
                &[&qualified],
            )
            .await?;
        let found: Vec<(String, String, String)> = constraints
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        let mut expected: Vec<(String, String, String)> = vec![
            (
                def.key_name.to_owned(),
                "u".to_owned(),
                format!("UNIQUE ({}, create_date)", def.key_column),
            ),
            (
                def.primary_key.to_owned(),
                "p".to_owned(),
                "PRIMARY KEY (id, create_date)".to_owned(),
            ),
        ];
        for check in def.checks {
            expected.push((
                check.name.to_owned(),
                "c".to_owned(),
                check.printed.to_owned(),
            ));
        }
        // the query orders by name (the `name` type sorts bytewise)
        expected.sort();
        if found != expected {
            return Err(EventSchemaProblem::ForgedConstraint(format!(
                "{qualified} has constraints {found:?}, expected {expected:?}"
            ))
            .into());
        }
        let others = self
            .query(
                "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = $1::text::regclass \
                 AND NOT tgisinternal), \
                 (SELECT count(*) FROM pg_rewrite WHERE ev_class = $1::text::regclass)",
                &[&qualified],
            )
            .await?;
        let (triggers, rules): (i64, i64) = (others[0].get(0), others[0].get(1));
        if triggers != 0 || rules != 0 {
            return Err(EventSchemaProblem::ForgedConstraint(format!(
                "{qualified} has {triggers} trigger(s) and {rules} rule(s), expected none"
            ))
            .into());
        }
        Ok(())
    }

    async fn check_event_indexes(&mut self, def: &TableDef) -> Result<()> {
        let qualified = def.qualified();
        let rows = self
            .query(
                "SELECT pg_get_indexdef(indexrelid) FROM pg_index \
                 WHERE indrelid = $1::text::regclass ORDER BY 1",
                &[&qualified],
            )
            .await?;
        // sorted here, not by the server: its text ordering depends on the database collation
        let mut found: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        found.sort_unstable();
        let mut expected: Vec<&str> = def.indexes.to_vec();
        expected.sort_unstable();
        if found != expected {
            return Err(EventSchemaProblem::ForgedIndex(format!(
                "{qualified} has indexes {found:?}, expected {expected:?}"
            ))
            .into());
        }
        Ok(())
    }

    async fn check_event_sequence(&mut self, def: &TableDef) -> Result<()> {
        let sequence = format!("\"{SCHEMA}\".\"{}\"", def.sequence);
        let rows = self
            .query(
                "SELECT c.relkind::text, pg_get_userbyid(c.relowner)::text = current_user::text, \
                 s.seqtypid::regtype::text, s.seqincrement, s.seqcycle, \
                 EXISTS (SELECT FROM pg_depend d WHERE d.classid = 'pg_class'::regclass \
                   AND d.objid = c.oid AND d.refobjid = $2::text::regclass AND d.refobjsubid = 1 \
                   AND d.deptype = 'a') \
                 FROM pg_class c LEFT JOIN pg_sequence s ON s.seqrelid = c.oid \
                 WHERE c.oid = to_regclass($1)",
                &[&sequence, &def.qualified()],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(EventSchemaProblem::ForgedSequence(format!(
                "sequence {sequence} is missing"
            ))
            .into());
        };
        let (kind, owned, ty, increment, cycle, bound): (
            String,
            bool,
            Option<String>,
            Option<i64>,
            Option<bool>,
            bool,
        ) = (
            row.get(0),
            row.get(1),
            row.get(2),
            row.get(3),
            row.get(4),
            row.get(5),
        );
        if kind != "S" {
            return Err(EventSchemaProblem::ForgedRelation(format!(
                "{sequence} is a {} (relkind {kind}), expected a sequence",
                relkind_name(&kind)
            ))
            .into());
        }
        if !owned {
            return Err(EventSchemaProblem::ForgedOwner(format!(
                "sequence {sequence} is not owned by the installing role"
            ))
            .into());
        }
        if ty.as_deref() != Some("bigint") || increment != Some(1) || cycle != Some(false) || !bound
        {
            return Err(EventSchemaProblem::ForgedSequence(format!(
                "sequence {sequence} is {} with increment {increment:?}, cycle {cycle:?}, owned by \
                 the id column: {bound}; expected bigint, increment 1, no cycle, owned by id",
                ty.as_deref().unwrap_or("not a sequence")
            ))
            .into());
        }
        Ok(())
    }

    /// The window of days (today and [`EVENT_PREMAKE_DAYS`] after, UTC), then the missing
    /// partitions. Returns (created, already present) names in date order.
    async fn make_event_partitions(
        &mut self,
        def: &TableDef,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let rows = self
            .query(
                "SELECT to_char(d, 'YYYYMMDD'), to_char(d, 'YYYY-MM-DD') || ' 00:00:00+00', \
                 to_char(d + 1, 'YYYY-MM-DD') || ' 00:00:00+00' \
                 FROM (SELECT (now() AT TIME ZONE 'UTC')::date + g AS d \
                       FROM generate_series(0, $1::int) g) days ORDER BY 1",
                &[&i32::try_from(def.premake_days)
                    .map_err(|_| Error::programming("the premake window does not fit i32"))?],
            )
            .await?;
        let slots: Vec<Slot> = rows
            .iter()
            .map(|r| Slot {
                name: format!("{}_p{}", def.table, r.get::<_, String>(0)),
                from: r.get(1),
                to: r.get(2),
            })
            .collect();
        let taken = self
            .query(
                "SELECT c.relname::text, c.relkind::text, \
                 EXISTS (SELECT FROM pg_inherits i WHERE i.inhrelid = c.oid \
                   AND i.inhparent = $2::text::regclass) \
                 FROM pg_class c WHERE c.relnamespace = $1::text::regnamespace \
                 AND c.relname = ANY($3)",
                &[
                    &format!("\"{SCHEMA}\""),
                    &def.qualified(),
                    &slots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                ],
            )
            .await?;
        let mut created = Vec::new();
        let mut existing = Vec::new();
        for slot in &slots {
            let present = taken.iter().find(|r| r.get::<_, String>(0) == slot.name);
            match present {
                Some(row) if row.get::<_, bool>(2) => existing.push(slot.name.clone()),
                Some(row) => {
                    return Err(EventSchemaProblem::ForgedPartition(format!(
                        "\"{SCHEMA}\".\"{}\" exists (relkind {}) but is not a partition of \
                         \"{}\"",
                        slot.name,
                        def.table,
                        row.get::<_, String>(1)
                    ))
                    .into());
                }
                None => {
                    self.batch(&format!(
                        "CREATE TABLE \"{SCHEMA}\".\"{}\" PARTITION OF {} \
                         FOR VALUES FROM ('{}') TO ('{}')",
                        slot.name,
                        def.qualified(),
                        slot.from,
                        slot.to
                    ))
                    .await?;
                    created.push(slot.name.clone());
                }
            }
        }
        Ok((created, existing))
    }

    /// Every attached partition: a plain table of ours, named for one UTC day with that day as its
    /// range, no default partition.
    async fn check_event_partitions(&mut self, def: &TableDef) -> Result<()> {
        self.with_catalog_view(async |tx| {
            let rows = tx
                .query(
                    &format!(
                        "SELECT c.relname::text, c.relkind::text, \
                         pg_get_userbyid(c.relowner)::text = current_user::text, \
                         pg_get_expr(c.relpartbound, c.oid), \
                         CASE WHEN c.relname ~ '^{table}_p[0-9]{{8}}$' THEN \
                           to_char(to_date(substr(c.relname, {from}), 'YYYYMMDD'), 'YYYY-MM-DD') END, \
                         CASE WHEN c.relname ~ '^{table}_p[0-9]{{8}}$' THEN \
                           to_char(to_date(substr(c.relname, {from}), 'YYYYMMDD') + 1, 'YYYY-MM-DD') END \
                         FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
                         WHERE i.inhparent = $1::text::regclass ORDER BY c.relname",
                        table = def.table,
                        from = def.table.len() + 3
                    ),
                    &[&def.qualified()],
                )
                .await?;
            for row in &rows {
                let name: String = row.get(0);
                let kind: String = row.get(1);
                let owned: bool = row.get(2);
                let bound: String = row.get(3);
                let day: Option<String> = row.get(4);
                let next: Option<String> = row.get(5);
                if bound == "DEFAULT" {
                    return Err(EventSchemaProblem::DefaultPartition(format!(
                        "\"{SCHEMA}\".\"{name}\" is a default partition (it forbids DETACH \
                         PARTITION ... CONCURRENTLY and would catch rows the missing-partition \
                         error must reject)"
                    ))
                    .into());
                }
                if kind != "r" {
                    return Err(EventSchemaProblem::ForgedPartition(format!(
                        "\"{SCHEMA}\".\"{name}\" is a {} (relkind {kind}), expected a plain table",
                        relkind_name(&kind)
                    ))
                    .into());
                }
                if !owned {
                    return Err(EventSchemaProblem::ForgedOwner(format!(
                        "partition \"{SCHEMA}\".\"{name}\" is not owned by the installing role"
                    ))
                    .into());
                }
                let (Some(day), Some(next)) = (day, next) else {
                    return Err(EventSchemaProblem::ForgedPartition(format!(
                        "\"{SCHEMA}\".\"{name}\" is not named {}_pYYYYMMDD",
                        def.table
                    ))
                    .into());
                };
                let expected =
                    format!("FOR VALUES FROM ('{day} 00:00:00+00') TO ('{next} 00:00:00+00')");
                if bound != expected {
                    return Err(EventSchemaProblem::ForgedPartition(format!(
                        "\"{SCHEMA}\".\"{name}\" has range {bound}, expected {expected}"
                    ))
                    .into());
                }
            }
            Ok(())
        })
        .await
    }
}

fn nullability(not_null: bool) -> &'static str {
    if not_null { "NOT NULL" } else { "nullable" }
}

fn relkind_name(kind: &str) -> &'static str {
    match kind {
        "r" => "plain table",
        "p" => "partitioned table",
        "v" => "view",
        "m" => "materialized view",
        "f" => "foreign table",
        "S" => "sequence",
        "i" | "I" => "index",
        "c" => "composite type",
        _ => "relation of another kind",
    }
}

/// The statements creating the sequence, the table and its indexes (the schema exists by then).
fn create_ddl(def: &TableDef) -> String {
    let columns: Vec<String> = def
        .columns
        .iter()
        .map(|c| {
            format!(
                "\"{}\" {}{}{}",
                c.name,
                c.ty,
                if c.not_null { " NOT NULL" } else { "" },
                c.default_ddl
                    .map(|d| format!(" DEFAULT {d}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    let checks: String = def
        .checks
        .iter()
        .map(|c| format!(", CONSTRAINT \"{}\" CHECK ({})", c.name, c.ddl))
        .collect();
    let extra: String = def
        .extra_index_ddl
        .iter()
        .map(|ddl| format!(";\n{ddl}"))
        .collect();
    format!(
        "CREATE SEQUENCE \"{SCHEMA}\".\"{sequence}\" AS bigint;\n\
         CREATE TABLE {qualified} ({columns}, CONSTRAINT \"{primary_key}\" PRIMARY KEY \
         (id, create_date), CONSTRAINT \"{key_name}\" UNIQUE ({key_column}, create_date){checks}) PARTITION BY RANGE (create_date);\n\
         ALTER SEQUENCE \"{SCHEMA}\".\"{sequence}\" OWNED BY {qualified}.\"id\";\n\
         CREATE INDEX \"{cursor}\" ON {qualified} (xid, id);\n\
         CREATE INDEX \"{time}\" ON {qualified} USING brin (create_date){extra}",
        sequence = def.sequence,
        qualified = def.qualified(),
        columns = columns.join(", "),
        primary_key = def.primary_key,
        key_name = def.key_name,
        key_column = def.key_column,
        cursor = def.cursor_index(),
        time = def.time_index(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> AdmissionFacts {
        AdmissionFacts {
            version_num: 180_006,
            uuidv7_returns: Some("uuid".to_owned()),
            xact_id_returns: Some("xid8".to_owned()),
            transaction_timestamp_returns: Some("timestamp with time zone".to_owned()),
            xid8_exists: true,
            schema_exists: false,
            can_create_schema: true,
        }
    }

    #[test]
    fn admission_accepts_a_postgres_18_server() {
        assert_eq!(check_admission(&facts()), Ok(()));
    }

    #[test]
    fn admission_refuses_an_old_server_a_missing_function_and_no_create_right() {
        let mut old = facts();
        old.version_num = 170_009;
        let mut no_function = facts();
        no_function.uuidv7_returns = None;
        let mut wrong_type = facts();
        wrong_type.xact_id_returns = Some("bigint".to_owned());
        let mut no_right = facts();
        no_right.can_create_schema = false;
        for (facts, needle) in [
            (old, "needs 18 or later"),
            (no_function, "pg_catalog.uuidv7() does not exist"),
            (wrong_type, "returns bigint, expected xid8"),
            (no_right, "may not create it"),
        ] {
            let problem = check_admission(&facts).expect_err("must refuse");
            assert_eq!(problem.code(), "admission");
            assert!(problem.detail().contains(needle), "{problem}");
        }
        let mut existing_schema = facts();
        existing_schema.schema_exists = true;
        existing_schema.can_create_schema = false;
        assert_eq!(check_admission(&existing_schema), Ok(()));
    }

    #[test]
    fn ddl_matches_the_expected_defaults() {
        let ddl = create_ddl(&OUTBOX);
        for column in OUTBOX_COLUMNS {
            if let Some(default) = column.default_ddl {
                assert!(ddl.contains(default), "{ddl}");
            }
        }
        assert!(ddl.contains("PARTITION BY RANGE (create_date)"));
        assert!(!ddl.to_lowercase().contains("default partition"));
    }

    #[test]
    fn schema_preserves_operation_order_and_actor_discriminator() {
        for (name, ty) in [
            ("principal_kind", "text"),
            ("actor_kind", "text"),
            ("operation_id", "uuid"),
            ("tx_ordinal", "bigint"),
        ] {
            let column = OUTBOX_COLUMNS
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing required capture column {name}"));
            assert_eq!(column.ty, ty);
            assert!(column.not_null);
            assert!(
                column.default.is_none(),
                "{name} is supplied by framework capture"
            );
        }
        assert!(create_ddl(&OUTBOX).contains("UNIQUE (event_id, create_date)"));
    }
}
