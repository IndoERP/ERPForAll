// SPDX-License-Identifier: MIT OR Apache-2.0
//! Raw native declarations of addon-owned SQL objects (contract §2): const-built,
//! `&'static` data linked into the binary or passed to the builder by tests. Nothing here is
//! validated; [`crate::RegistryBuilder::build`] validates and resolves them into a
//! [`crate::SqlObjectGraph`].

/// The set of SQL object keys an addon expects to be registered, required at the build site
/// ([`crate::RegistryBuilder::require_sql_objects`]).
#[derive(Debug, Clone, Copy)]
pub struct SqlObjectManifest {
    pub(crate) addon: &'static str,
    pub(crate) expected: &'static [&'static str],
}

impl SqlObjectManifest {
    /// Addon `addon` expects exactly the local keys `expected` to be declared.
    pub const fn new(addon: &'static str, expected: &'static [&'static str]) -> Self {
        Self { addon, expected }
    }
}

/// One SQL object declared by a native addon: owner addon, local key and kind.
#[derive(Debug, Clone, Copy)]
pub struct SqlObjectDecl {
    pub(crate) addon: &'static str,
    pub(crate) key: &'static str,
    pub(crate) kind: SqlObjectKindDecl,
}

impl SqlObjectDecl {
    /// The owner addon.
    pub const fn addon(&self) -> &'static str {
        self.addon
    }

    /// The local key.
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// A function owned by `addon` under the local `key`.
    pub const fn function(addon: &'static str, key: &'static str, decl: SqlFunctionDecl) -> Self {
        Self {
            addon,
            key,
            kind: SqlObjectKindDecl::Function(decl),
        }
    }

    /// A trigger owned by `addon` under the local `key`.
    pub const fn trigger(addon: &'static str, key: &'static str, decl: SqlTriggerDecl) -> Self {
        Self {
            addon,
            key,
            kind: SqlObjectKindDecl::Trigger(decl),
        }
    }

    /// A condition owned by `addon` under the local `key`.
    pub const fn condition(addon: &'static str, key: &'static str, decl: SqlConditionDecl) -> Self {
        Self {
            addon,
            key,
            kind: SqlObjectKindDecl::Condition(decl),
        }
    }

    /// A restricted table grant policy owned by `addon` under the local `key` (A2).
    pub const fn table_grant(
        addon: &'static str,
        key: &'static str,
        decl: SqlTableGrantDecl,
    ) -> Self {
        Self {
            addon,
            key,
            kind: SqlObjectKindDecl::TableGrant(decl),
        }
    }
}

/// What a raw declaration declares. Foreign keys, checks and table options come with later slices.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SqlObjectKindDecl {
    /// A function.
    Function(SqlFunctionDecl),
    /// A trigger.
    Trigger(SqlTriggerDecl),
    /// A condition (SQLSTATE plus identifier with a static public message).
    Condition(SqlConditionDecl),
    /// A restricted table grant policy.
    TableGrant(SqlTableGrantDecl),
}

/// The four role bindings amendment A2. Symbolic: the operator configures which
/// database role each one is; a declaration names a binding, never a role. Closed: a binding an
/// addon could name but core does not know cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqlBinding {
    /// The role schema sync runs as: owns the tables, triggers and invoker functions.
    SchemaOwner,
    /// A NOLOGIN role that owns the `SECURITY DEFINER` functions and holds only the table
    /// privileges its declared grant policies list.
    Definer,
    /// The maintenance pool's login role: EXECUTE on declared entry points only.
    Maintenance,
    /// The request (runtime) role.
    Request,
}

impl SqlBinding {
    /// The binding's name as errors and documentation spell it (`schema_owner`, `definer`,
    /// `maintenance`, `request`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::SchemaOwner => "schema_owner",
            Self::Definer => "definer",
            Self::Maintenance => "maintenance",
            Self::Request => "request",
        }
    }
}

impl std::fmt::Display for SqlBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A table privilege a grant policy may list (contract §5): a closed set. `TRUNCATE`,
/// `REFERENCES`, `TRIGGER`, `MAINTAIN` and DDL cannot be granted to a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqlTableOp {
    /// `SELECT`
    Select,
    /// `INSERT`
    Insert,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
}

impl SqlTableOp {
    /// The SQL privilege name.
    pub const fn sql(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// A raw restricted grant policy: on the table of `model`, `binding` holds exactly `ops`. A table
/// with at least one policy is restricted: every configured binding without a policy on it holds
/// nothing there, and PUBLIC holds nothing.
#[derive(Debug, Clone, Copy)]
pub struct SqlTableGrantDecl {
    pub(crate) model: &'static str,
    pub(crate) binding: SqlBinding,
    pub(crate) ops: &'static [SqlTableOp],
}

impl SqlTableGrantDecl {
    /// `binding` holds exactly `ops` on the table of `model`.
    pub const fn new(model: &'static str, binding: SqlBinding, ops: &'static [SqlTableOp]) -> Self {
        Self {
            model,
            binding,
            ops,
        }
    }
}

/// A builtin PostgreSQL scalar type (A3; a wider vocabulary needs an amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqlScalar {
    /// `boolean`
    Boolean,
    /// `smallint`
    SmallInt,
    /// `integer`
    Integer,
    /// `bigint`
    BigInt,
    /// `numeric`
    Numeric,
    /// `real`
    Real,
    /// `double precision`
    Double,
    /// `text`
    Text,
    /// `date`
    Date,
    /// `time`
    Time,
    /// `timestamp`
    Timestamp,
    /// `timestamptz`
    TimestampTz,
    /// `interval`
    Interval,
    /// `uuid`
    Uuid,
    /// `jsonb`
    Jsonb,
    /// `bytea`
    Bytea,
}

/// A function argument type: a builtin scalar or a one-dimensional array of one (A3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqlType {
    /// A builtin scalar.
    Scalar(SqlScalar),
    /// A one-dimensional array of a builtin scalar.
    Array(SqlScalar),
}

/// A function result (A3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlResult {
    /// A builtin scalar.
    Scalar(SqlScalar),
    /// `void`
    Void,
    /// `trigger`: the function can only be used by a trigger.
    Trigger,
    /// `SETOF` a builtin scalar.
    SetOf(SqlScalar),
}

/// The language of a function body; C, internal and untrusted languages are not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlLanguage {
    /// `sql`
    Sql,
    /// `plpgsql`
    PlPgSql,
}

/// Function volatility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlVolatility {
    /// `IMMUTABLE`
    Immutable,
    /// `STABLE`
    Stable,
    /// `VOLATILE`
    Volatile,
}

/// Function security mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlSecurity {
    /// `SECURITY INVOKER`
    Invoker,
    /// `SECURITY DEFINER`
    Definer,
}

/// A declared dependency of a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SqlDep {
    /// Another SQL object, by owner addon and local key.
    Object {
        /// Owner addon.
        addon: &'static str,
        /// Local key.
        key: &'static str,
    },
    /// The table of a model, by model name.
    Model(&'static str),
}

/// A raw function declaration.
#[derive(Debug, Clone, Copy)]
pub struct SqlFunctionDecl {
    pub(crate) schema: &'static str,
    pub(crate) name: &'static str,
    pub(crate) language: SqlLanguage,
    pub(crate) args: &'static [SqlType],
    pub(crate) result: SqlResult,
    pub(crate) body: &'static str,
    pub(crate) volatility: SqlVolatility,
    pub(crate) security: SqlSecurity,
    pub(crate) deps: &'static [SqlDep],
    pub(crate) derived: Option<(SqlColumnSet, SqlBodyRender)>,
    pub(crate) execute: &'static [SqlBinding],
}

impl SqlFunctionDecl {
    /// Function `name` in schema `public`, no arguments, `VOLATILE`, `SECURITY INVOKER`, no
    /// dependencies.
    pub const fn new(
        name: &'static str,
        language: SqlLanguage,
        result: SqlResult,
        body: &'static str,
    ) -> Self {
        Self {
            schema: "public",
            name,
            language,
            args: &[],
            result,
            body,
            volatility: SqlVolatility::Volatile,
            security: SqlSecurity::Invoker,
            deps: &[],
            derived: None,
            execute: &[],
        }
    }

    /// A catalog-derived function: its body is produced at schema sync by
    /// `render` from the compared columns of the table of `columns`' model (the ordinary user
    /// columns in `attnum` order, minus the allow-list, generated and dropped columns excluded),
    /// and re-rendered in the same transaction as every column change of that table. The
    /// function depends on the model's table and installs after the tables. `render` is trusted
    /// native code and must be deterministic.
    pub const fn catalog_derived(
        name: &'static str,
        language: SqlLanguage,
        result: SqlResult,
        columns: SqlColumnSet,
        render: SqlBodyRender,
    ) -> Self {
        let mut decl = Self::new(name, language, result, "");
        decl.derived = Some((columns, render));
        decl
    }

    /// The schema the function lives in.
    #[must_use]
    pub const fn schema(mut self, schema: &'static str) -> Self {
        self.schema = schema;
        self
    }

    /// The argument types, in order.
    #[must_use]
    pub const fn args(mut self, args: &'static [SqlType]) -> Self {
        self.args = args;
        self
    }

    /// The volatility.
    #[must_use]
    pub const fn volatility(mut self, volatility: SqlVolatility) -> Self {
        self.volatility = volatility;
        self
    }

    /// The security mode.
    #[must_use]
    pub const fn security(mut self, security: SqlSecurity) -> Self {
        self.security = security;
        self
    }

    /// The objects and model tables the body uses.
    #[must_use]
    pub const fn deps(mut self, deps: &'static [SqlDep]) -> Self {
        self.deps = deps;
        self
    }

    /// The bindings granted EXECUTE (A2). PUBLIC never is, and nothing is by default:
    /// a function no binding may execute is still run by its triggers.
    #[must_use]
    pub const fn grant_execute(mut self, bindings: &'static [SqlBinding]) -> Self {
        self.execute = bindings;
        self
    }
}

/// The body renderer of a catalog-derived function: compared columns in, body out.
pub type SqlBodyRender = fn(&[SqlCompareColumn]) -> String;

/// The table identity and allow-list of a catalog-derived function (contract §2): the columns
/// of the allow-list are left out of the comparison.
#[derive(Debug, Clone, Copy)]
pub struct SqlColumnSet {
    pub(crate) model: &'static str,
    pub(crate) allow: &'static [&'static str],
}

impl SqlColumnSet {
    /// The table of `model`, with the stored column fields `allow` left out of the comparison.
    pub const fn new(model: &'static str, allow: &'static [&'static str]) -> Self {
        Self { model, allow }
    }

    /// The model whose table is compared.
    pub const fn model(&self) -> &'static str {
        self.model
    }

    /// The allow-listed column fields.
    pub const fn allow(&self) -> &'static [&'static str] {
        self.allow
    }
}

/// One compared column given to a [`SqlBodyRender`]: its catalog name and type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCompareColumn {
    name: String,
    sql_type: String,
}

impl SqlCompareColumn {
    /// Column `name` of catalog type `sql_type` (`format_type` of the column).
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sql_type: sql_type.into(),
        }
    }

    /// The column name, unquoted.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column name, quoted for SQL.
    pub fn quoted(&self) -> String {
        crate::ident::quote(&self.name)
    }

    /// The catalog type (`format_type(atttypid, atttypmod)`).
    pub fn sql_type(&self) -> &str {
        &self.sql_type
    }
}

/// Trigger timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTiming {
    /// `BEFORE`
    Before,
    /// `AFTER`
    After,
}

/// Trigger event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqlEvent {
    /// `INSERT`
    Insert,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
    /// `TRUNCATE`
    Truncate,
}

/// Trigger level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlLevel {
    /// `FOR EACH ROW`
    Row,
    /// `FOR EACH STATEMENT`
    Statement,
}

/// The deferral of a constraint trigger, always stated explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDeferral {
    /// `NOT DEFERRABLE`
    NotDeferrable,
    /// `DEFERRABLE INITIALLY IMMEDIATE`
    InitiallyImmediate,
    /// `DEFERRABLE INITIALLY DEFERRED`
    InitiallyDeferred,
}

/// A raw trigger declaration. The shape is deliberately general so that unsupported shapes are
/// representable and refused by validation naming the rule; validation maps a supported shape onto
/// one of the four closed [`crate::SqlTriggerShape`] variants.
#[derive(Debug, Clone, Copy)]
pub struct SqlTriggerDecl {
    pub(crate) name: &'static str,
    pub(crate) model: &'static str,
    pub(crate) function: (&'static str, &'static str),
    pub(crate) timing: SqlTiming,
    pub(crate) events: &'static [SqlEvent],
    pub(crate) level: SqlLevel,
    pub(crate) update_of: &'static [&'static str],
    pub(crate) old_table: Option<&'static str>,
    pub(crate) new_table: Option<&'static str>,
    pub(crate) constraint: Option<SqlDeferral>,
}

impl SqlTriggerDecl {
    /// Trigger `name` on the table of `model`, executing the function object
    /// `function` (`(addon, key)`): `timing` `events` `FOR EACH level`, with no column list, no
    /// transition tables and not a constraint trigger.
    pub const fn new(
        name: &'static str,
        model: &'static str,
        function: (&'static str, &'static str),
        timing: SqlTiming,
        events: &'static [SqlEvent],
        level: SqlLevel,
    ) -> Self {
        Self {
            name,
            model,
            function,
            timing,
            events,
            level,
            update_of: &[],
            old_table: None,
            new_table: None,
            constraint: None,
        }
    }

    /// An `UPDATE OF` column list.
    #[must_use]
    pub const fn update_of(mut self, columns: &'static [&'static str]) -> Self {
        self.update_of = columns;
        self
    }

    /// The `OLD TABLE AS` transition alias.
    #[must_use]
    pub const fn old_table(mut self, alias: &'static str) -> Self {
        self.old_table = Some(alias);
        self
    }

    /// The `NEW TABLE AS` transition alias.
    #[must_use]
    pub const fn new_table(mut self, alias: &'static str) -> Self {
        self.new_table = Some(alias);
        self
    }

    /// Make it a constraint trigger with an explicit deferral.
    #[must_use]
    pub const fn constraint(mut self, deferral: SqlDeferral) -> Self {
        self.constraint = Some(deferral);
        self
    }
}

/// A raw condition declaration: SQLSTATE, condition identifier and static public message.
#[derive(Debug, Clone, Copy)]
pub struct SqlConditionDecl {
    pub(crate) sqlstate: &'static str,
    pub(crate) identifier: &'static str,
    pub(crate) message: &'static str,
}

impl SqlConditionDecl {
    /// Condition `identifier` raised with `sqlstate`, shown with the static `message`.
    pub const fn new(
        sqlstate: &'static str,
        identifier: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            sqlstate,
            identifier,
            message,
        }
    }
}
