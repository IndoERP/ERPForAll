// SPDX-License-Identifier: MIT OR Apache-2.0
//! DDL text and canonical hash payloads of addon-owned SQL objects (slice B3; contract,
//! §4). Pure: the objects come validated from
//! [`erp_registry::SqlObjectGraph`]; every identifier is quoted, every body is dollar-quoted
//! with a tag proven absent from it, and every function gets the core-rendered search path of
//! amendment A1 (`pg_catalog`, the declared schemas, `pg_temp` last). `erp-orm` executes.

use erp_fields::{Error, Result};
use erp_registry::{
    SqlCompareColumn, SqlDeferral, SqlFunction, SqlLanguage, SqlObject, SqlObjectGraph,
    SqlObjectKind, SqlReference, SqlResult, SqlScalar, SqlSecurity, SqlTableOp, SqlTransition,
    SqlTrigger, SqlTriggerShape, SqlType, SqlVolatility,
};

use crate::quote;
use erp_registry::{SqlForeignKey, SqlForeignKeyAction, SqlObjectKind as Kind};

/// Version of the canonical payload format; a changed format needs a versioned migration.
pub const FORMAT_VERSION: i32 = 1;

fn scalar_sql(scalar: SqlScalar) -> &'static str {
    match scalar {
        SqlScalar::Boolean => "boolean",
        SqlScalar::SmallInt => "smallint",
        SqlScalar::Integer => "integer",
        SqlScalar::BigInt => "bigint",
        SqlScalar::Numeric => "numeric",
        SqlScalar::Real => "real",
        SqlScalar::Double => "double precision",
        SqlScalar::Text => "text",
        SqlScalar::Date => "date",
        SqlScalar::Time => "time",
        SqlScalar::Timestamp => "timestamp",
        SqlScalar::TimestampTz => "timestamptz",
        SqlScalar::Interval => "interval",
        SqlScalar::Uuid => "uuid",
        SqlScalar::Jsonb => "jsonb",
        SqlScalar::Bytea => "bytea",
    }
}

/// The SQL spelling of an argument type: `integer`, `text[]`.
pub fn type_sql(ty: SqlType) -> String {
    match ty {
        SqlType::Scalar(s) => scalar_sql(s).to_owned(),
        SqlType::Array(s) => format!("{}[]", scalar_sql(s)),
    }
}

fn result_sql(result: SqlResult) -> String {
    match result {
        SqlResult::Scalar(s) => scalar_sql(s).to_owned(),
        SqlResult::Void => "void".to_owned(),
        SqlResult::Trigger => "trigger".to_owned(),
        SqlResult::SetOf(s) => format!("SETOF {}", scalar_sql(s)),
    }
}

fn arg_list(f: &SqlFunction) -> String {
    let args: Vec<String> = f.args().iter().map(|t| type_sql(*t)).collect();
    args.join(", ")
}

/// The function's database identity: `"schema"."name"(integer, text[])`, also accepted by
/// `to_regprocedure`.
pub fn signature(f: &SqlFunction) -> String {
    format!("{}.{}({})", quote(f.schema()), quote(f.name()), arg_list(f))
}

/// The declared schemas of A1 for the function `object` (plan D5): its own schema, then the
/// schema of each declared dependency in order (another function's schema; `current_schema`
/// for a model table), de-duplicated in first-seen order. Never `pg_catalog` or `pg_temp`:
/// validation refuses `pg_*` schemas, and [`search_path_clause`] adds those two itself.
pub fn search_path_schemas(
    graph: &SqlObjectGraph,
    object: &SqlObject,
    current_schema: &str,
) -> Vec<String> {
    let mut schemas: Vec<String> = Vec::new();
    let mut add = |schema: &str| {
        if !schemas.iter().any(|s| s == schema) {
            schemas.push(schema.to_owned());
        }
    };
    if let SqlObjectKind::Function(f) = object.kind() {
        add(f.schema());
    }
    for reference in object.references() {
        match reference {
            SqlReference::Object(id) => {
                let target = graph.get(id.addon(), id.key()).map(SqlObject::kind);
                if let Some(SqlObjectKind::Function(f)) = target {
                    add(f.schema());
                }
            }
            SqlReference::Model(_) => add(current_schema),
            // a reference kind of a later slice brings no schema of its own
            _ => {}
        }
    }
    schemas
}

/// `SET search_path = pg_catalog, "<schema>", …, pg_temp` (A1): core-rendered, `pg_temp`
/// always listed and always last.
pub fn search_path_clause(schemas: &[String]) -> String {
    let mut parts = vec!["pg_catalog".to_owned()];
    parts.extend(schemas.iter().map(|s| quote(s)));
    parts.push("pg_temp".to_owned());
    format!("SET search_path = {}", parts.join(", "))
}

/// `body` in a dollar quote whose tag (`$erp$`, `$erp1$`, …) occurs neither in the body nor
/// across its end, so the first occurrence of the tag after the opening one closes the quote.
///
/// # Errors
///
/// `Programming` when no tag fits, which cannot happen: each position of the body rules out
/// at most one candidate and one more candidate is tried than there are positions.
pub fn dollar_quote(body: &str) -> Result<String> {
    for n in 0..=body.len().saturating_add(1) {
        let tag = if n == 0 {
            "$erp$".to_owned()
        } else {
            format!("$erp{n}$")
        };
        let closed = format!("{body}{tag}");
        if closed.find(&tag) == Some(body.len()) {
            return Ok(format!("{tag}{closed}"));
        }
    }
    Err(Error::programming(
        "no dollar-quote tag is absent from the function body",
    ))
}

/// `CREATE [OR REPLACE] FUNCTION` of `f` with `body` and the A1 search path of `schemas`.
/// `replace` is for a body-only change of an installed function (same signature, result and
/// security), which keeps its OID.
///
/// # Errors
///
/// As [`dollar_quote`].
pub fn create_function(
    f: &SqlFunction,
    body: &str,
    schemas: &[String],
    replace: bool,
) -> Result<String> {
    let (language, volatility) = (language_sql(f), volatility_sql(f));
    Ok(format!(
        "CREATE {}FUNCTION {}({}) RETURNS {} LANGUAGE {language} {volatility} SECURITY {} {} AS \
         {}",
        if replace { "OR REPLACE " } else { "" },
        format_args!("{}.{}", quote(f.schema()), quote(f.name())),
        arg_list(f),
        result_sql(f.result()),
        security_sql(f.security()),
        search_path_clause(schemas),
        dollar_quote(body)?
    ))
}

/// `REVOKE ALL ON FUNCTION … FROM PUBLIC`: sent in the batch that creates the function (A2), so
/// no statement of the transaction sees it executable by PUBLIC.
pub fn revoke_public(f: &SqlFunction) -> String {
    format!("REVOKE ALL ON FUNCTION {} FROM PUBLIC", signature(f))
}

/// The EXECUTE reconciliation of `f`: every privilege of PUBLIC and of the `bound` roles revoked,
/// then EXECUTE granted to `execute` (a subset of `bound`). Roles come quoted from the caller's
/// role validation; no declaration text reaches the statement but validated identifiers.
pub fn function_grants(f: &SqlFunction, bound: &[String], execute: &[String]) -> String {
    let sig = signature(f);
    let mut sql = format!("REVOKE ALL ON FUNCTION {sig} FROM PUBLIC");
    for role in bound {
        sql.push_str(&format!(";\nREVOKE ALL ON FUNCTION {sig} FROM {role}"));
    }
    for role in execute {
        sql.push_str(&format!(";\nGRANT EXECUTE ON FUNCTION {sig} TO {role}"));
    }
    sql
}

/// The ownership transfer of a `SECURITY DEFINER` function to the quoted `definer` role (A2).
/// PostgreSQL requires the new owner to hold CREATE on the function's schema, so the definer holds
/// it for the one statement and loses it again in the same batch.
pub fn transfer_to_definer(f: &SqlFunction, definer: &str) -> String {
    let schema = quote(f.schema());
    format!(
        "GRANT CREATE ON SCHEMA {schema} TO {definer};\nALTER FUNCTION {} OWNER TO \
         {definer};\nREVOKE CREATE ON SCHEMA {schema} FROM {definer}",
        signature(f)
    )
}

/// The reconciliation of a restricted table: every privilege of PUBLIC and of the `bound` roles
/// revoked, then each `(role, operations)` of `grants` granted. Roles come quoted.
pub fn table_grants(table: &str, bound: &[String], grants: &[(String, &[SqlTableOp])]) -> String {
    let table = quote(table);
    let mut sql = format!("REVOKE ALL ON TABLE {table} FROM PUBLIC");
    for role in bound {
        sql.push_str(&format!(";\nREVOKE ALL ON TABLE {table} FROM {role}"));
    }
    for (role, ops) in grants {
        let ops: Vec<&str> = ops.iter().map(|op| op.sql()).collect();
        sql.push_str(&format!(
            ";\nGRANT {} ON TABLE {table} TO {role}",
            ops.join(", ")
        ));
    }
    sql
}

/// Fixed spellings, also in the canonical payload (never Rust variant names).
fn language_sql(f: &SqlFunction) -> &'static str {
    match f.language() {
        SqlLanguage::Sql => "sql",
        SqlLanguage::PlPgSql => "plpgsql",
    }
}

fn volatility_sql(f: &SqlFunction) -> &'static str {
    match f.volatility() {
        SqlVolatility::Immutable => "IMMUTABLE",
        SqlVolatility::Stable => "STABLE",
        SqlVolatility::Volatile => "VOLATILE",
    }
}

fn security_sql(security: SqlSecurity) -> &'static str {
    match security {
        SqlSecurity::Invoker => "INVOKER",
        SqlSecurity::Definer => "DEFINER",
    }
}

/// `CREATE [CONSTRAINT] TRIGGER` of `t` on its model's table, executing `function` (the
/// function object `t` references).
pub fn create_trigger(t: &SqlTrigger, function: &SqlFunction) -> String {
    let table = quote(t.model().table());
    let name = quote(t.name());
    // a trigger function takes no arguments (validated), so its signature is the call
    let run = format!("EXECUTE FUNCTION {}", signature(function));
    match t.shape() {
        SqlTriggerShape::AfterStatement(transition) => {
            let (event, referencing) = match transition {
                SqlTransition::Insert { new_table } => {
                    ("INSERT", format!("NEW TABLE AS {}", quote(new_table)))
                }
                SqlTransition::Update {
                    old_table,
                    new_table,
                } => (
                    "UPDATE",
                    format!(
                        "OLD TABLE AS {} NEW TABLE AS {}",
                        quote(old_table),
                        quote(new_table)
                    ),
                ),
                SqlTransition::Delete { old_table } => {
                    ("DELETE", format!("OLD TABLE AS {}", quote(old_table)))
                }
            };
            format!(
                "CREATE TRIGGER {name} AFTER {event} ON {table} REFERENCING {referencing} FOR \
                 EACH STATEMENT {run}"
            )
        }
        SqlTriggerShape::BeforeTruncate => {
            format!("CREATE TRIGGER {name} BEFORE TRUNCATE ON {table} FOR EACH STATEMENT {run}")
        }
        SqlTriggerShape::BeforeInsertRow => {
            format!("CREATE TRIGGER {name} BEFORE INSERT ON {table} FOR EACH ROW {run}")
        }
        SqlTriggerShape::AfterInsertConstraintRow(deferral) => format!(
            "CREATE CONSTRAINT TRIGGER {name} AFTER INSERT ON {table} {} FOR EACH ROW {run}",
            deferral_sql(deferral)
        ),
    }
}

fn deferral_sql(deferral: SqlDeferral) -> &'static str {
    match deferral {
        SqlDeferral::NotDeferrable => "NOT DEFERRABLE",
        SqlDeferral::InitiallyImmediate => "DEFERRABLE INITIALLY IMMEDIATE",
        SqlDeferral::InitiallyDeferred => "DEFERRABLE INITIALLY DEFERRED",
    }
}

/// `ALTER TABLE … ENABLE ALWAYS TRIGGER …`: integrity triggers also fire under
/// `session_replication_role = replica`.
pub fn enable_always(t: &SqlTrigger) -> String {
    format!(
        "ALTER TABLE {} ENABLE ALWAYS TRIGGER {}",
        quote(t.model().table()),
        quote(t.name())
    )
}

/// The physical identity provenance keeps unique: a function's [`signature`], `trigger
/// "<name>" on "<table>"`, or `condition <SQLSTATE> "<identifier>"`.
pub fn physical_identity(object: &SqlObject) -> String {
    match object.kind() {
        SqlObjectKind::Function(f) => format!("function {}", signature(f)),
        SqlObjectKind::Trigger(t) => format!(
            "trigger {} on {}",
            quote(t.name()),
            quote(t.model().table())
        ),
        Kind::ForeignKey(f) => format!(
            "constraint {} on {}",
            quote(f.name()),
            quote(f.model().table())
        ),
        Kind::TableOptions(o) => format!("table {}", quote(o.model().table())),
        SqlObjectKind::Condition(c) => {
            format!("condition {} {}", c.sqlstate(), quote(c.identifier()))
        }
        // kinds of later slices have no physical identity yet
        _ => format!("object {}", object.id()),
    }
}

/// The shape components of `object`, labelled as a shape-change refusal names them.
fn shape(object: &SqlObject) -> Vec<(&'static str, Option<String>)> {
    let s = |v: &str| Some(v.to_owned());
    match object.kind() {
        SqlObjectKind::Function(f) => vec![
            ("kind", s("function")),
            ("function schema", s(f.schema())),
            ("function name", s(f.name())),
            ("argument types", s(&arg_list(f))),
            ("result type", s(&result_sql(f.result()))),
            ("security mode", s(security_sql(f.security()))),
        ],
        SqlObjectKind::Trigger(t) => {
            let (timing, event, level, deferral, old, new) = match t.shape() {
                SqlTriggerShape::AfterStatement(transition) => {
                    let (event, old, new) = match transition {
                        SqlTransition::Insert { new_table } => ("INSERT", None, s(new_table)),
                        SqlTransition::Update {
                            old_table,
                            new_table,
                        } => ("UPDATE", s(old_table), s(new_table)),
                        SqlTransition::Delete { old_table } => ("DELETE", s(old_table), None),
                    };
                    ("AFTER", event, "STATEMENT", None, old, new)
                }
                SqlTriggerShape::BeforeTruncate => {
                    ("BEFORE", "TRUNCATE", "STATEMENT", None, None, None)
                }
                SqlTriggerShape::BeforeInsertRow => ("BEFORE", "INSERT", "ROW", None, None, None),
                SqlTriggerShape::AfterInsertConstraintRow(d) => {
                    ("AFTER", "INSERT", "ROW", s(deferral_sql(d)), None, None)
                }
            };
            vec![
                ("kind", s("trigger")),
                ("trigger table", s(t.model().table())),
                ("trigger name", s(t.name())),
                ("trigger timing", s(timing)),
                ("trigger event", s(event)),
                ("trigger level", s(level)),
                ("deferral", deferral),
                ("OLD TABLE alias", old),
                ("NEW TABLE alias", new),
                ("trigger function", s(&t.function().to_string())),
            ]
        }
        Kind::ForeignKey(f) => vec![
            ("kind", s("foreign_key")),
            ("constraint table", s(f.model().table())),
            ("constraint name", s(f.name())),
            ("local columns", s(&f.columns().join(","))),
            ("target table", s(f.target().table())),
            ("target columns", s(&f.target_columns().join(","))),
            (
                "column types",
                s(&f.column_shapes()
                    .iter()
                    .map(|c| c.ty().sql())
                    .collect::<Vec<_>>()
                    .join(",")),
            ),
            (
                "column nullability",
                s(&payload(
                    &f.column_shapes()
                        .iter()
                        .flat_map(|c| {
                            [
                                Some(c.local_required().to_string()),
                                Some(c.target_required().to_string()),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )),
            ),
            (
                "target key",
                s(&match f.target_key() {
                    erp_registry::SqlForeignKeyTarget::PrimaryKey => "primary_key:id".to_owned(),
                    erp_registry::SqlForeignKeyTarget::Key(name) => format!("key:{name}"),
                }),
            ),
            ("delete action", s(&delete_action(f.on_delete()))),
            ("deferral", s(deferral_sql(f.deferral()))),
        ],
        Kind::TableOptions(o) => vec![
            ("kind", s("table_options")),
            ("table", s(o.model().table())),
            ("table persistence", s(o.persistence().catalog())),
        ],
        SqlObjectKind::Condition(_) => vec![("kind", s("condition"))],
        _ => vec![("kind", s(&format!("unsupported {}", object.id())))],
    }
}

/// The canonical payload of `values` (contract §4): `erp-sqlobj:<format>:<byte length>:<JSON
/// array>`, with fixed positions, explicit nulls and exact UTF-8 text.
pub fn payload(values: &[Option<String>]) -> String {
    let json = serde_json::Value::from(values.to_vec()).to_string();
    format!("erp-sqlobj:{FORMAT_VERSION}:{}:{json}", json.len())
}

/// The canonical payload of the shape of `object`, the comparator of a shape change.
pub fn shape_payload(object: &SqlObject) -> String {
    let values: Vec<Option<String>> = shape(object).into_iter().map(|(_, v)| v).collect();
    payload(&values)
}

/// The canonical payload of everything `object` declares: its shape, then for a function the
/// language, volatility, the A1 search path of `schemas` and `body` (exact bytes; for a
/// catalog-derived function the model, allow-list and the body rendered for no columns, so a
/// changed renderer is a changed declaration), for a condition its SQLSTATE, identifier and
/// message and explicit classification.
pub fn declaration_payload(object: &SqlObject, body: &str, schemas: &[String]) -> String {
    let mut values: Vec<Option<String>> = shape(object).into_iter().map(|(_, v)| v).collect();
    match object.kind() {
        SqlObjectKind::Function(f) => {
            values.push(Some(language_sql(f).to_owned()));
            values.push(Some(volatility_sql(f).to_owned()));
            values.push(Some(search_path_clause(schemas)));
            // the ordered dependency identities: a dependency-only change is a changed declaration
            let deps: Vec<String> = object.references().iter().map(reference_identity).collect();
            values.push(Some(deps.join(",")));
            match f.column_set() {
                Some(columns) => {
                    values.push(Some(columns.model().to_owned()));
                    values.push(Some(columns.allow().join(",")));
                    values.push(Some(f.render(&[])));
                }
                None => values.push(Some(body.to_owned())),
            }
        }
        SqlObjectKind::Condition(c) => {
            values.push(Some(c.sqlstate().to_owned()));
            values.push(Some(c.identifier().to_owned()));
            values.push(Some(c.message().to_owned()));
            values.push(Some(c.kind().name().to_owned()));
        }
        _ => {}
    }
    payload(&values)
}

/// `object:<addon>/<key>` or `model:<model>`, as provenance records the edge.
pub fn reference_identity(reference: &SqlReference) -> String {
    match reference {
        SqlReference::Object(id) => format!("object:{id}"),
        SqlReference::Model(m) => format!("model:{}", m.model()),
        // a reference kind of a later slice is named by its debug form until that slice
        other => format!("other:{other:?}"),
    }
}

/// The canonical payload of the compared columns of a catalog-derived body (name and catalog
/// type, in order).
pub fn columns_payload(columns: &[SqlCompareColumn]) -> String {
    let values: Vec<Option<String>> = columns
        .iter()
        .flat_map(|c| [Some(c.name().to_owned()), Some(c.sql_type().to_owned())])
        .collect();
    payload(&values)
}

/// The label of the first shape component of `object` that differs from the recorded shape
/// payload `recorded` (`argument types`, `trigger event`, …), or `None` when the shape is the
/// same.
///
/// # Errors
///
/// `Database` when `recorded` is not a canonical payload of [`FORMAT_VERSION`] (an unreadable
/// catalog is not a changed shape).
pub fn shape_change(object: &SqlObject, recorded: &str) -> Result<Option<&'static str>> {
    let unreadable = || {
        Error::new(
            erp_fields::ErrorKind::Database,
            format!(
                "SQL object '{}': the recorded shape is not a canonical payload of format \
                 {FORMAT_VERSION}",
                object.id()
            ),
        )
    };
    let prefix = format!("erp-sqlobj:{FORMAT_VERSION}:");
    let (len, json) = recorded
        .strip_prefix(&prefix)
        .and_then(|rest| rest.split_once(':'))
        .ok_or_else(unreadable)?;
    if len.parse::<usize>().ok() != Some(json.len()) {
        return Err(unreadable());
    }
    let values: Vec<Option<String>> = serde_json::from_str(json).map_err(|_| unreadable())?;
    let current = shape(object);
    if values.first() != current.first().map(|(_, v)| v) || values.len() != current.len() {
        return Ok(Some("kind"));
    }
    Ok(current
        .iter()
        .zip(&values)
        .find(|((_, now), was)| now != *was)
        .map(|((label, _), _)| *label))
}

/// Quoted foreign-key DDL, optionally not validated until the existing validation phase.
pub fn create_foreign_key(f: &SqlForeignKey, not_valid: bool) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} {}{}",
        quote(f.model().table()),
        quote(f.name()),
        quoted_columns(f.columns()),
        quote(f.target().table()),
        quoted_columns(f.target_columns()),
        delete_action(f.on_delete()),
        deferral_sql(f.deferral()),
        if not_valid { " NOT VALID" } else { "" }
    )
}
fn quoted_columns(columns: &[&str]) -> String {
    columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ")
}
fn delete_action(action: SqlForeignKeyAction) -> String {
    match action {
        SqlForeignKeyAction::NoAction => "NO ACTION".to_owned(),
        SqlForeignKeyAction::Restrict => "RESTRICT".to_owned(),
        SqlForeignKeyAction::Cascade => "CASCADE".to_owned(),
        SqlForeignKeyAction::SetNull(columns) => format!("SET NULL ({})", quoted_columns(columns)),
    }
}

/// Restrictive removal of a validated native SQL object. Metadata-only kinds have no DDL.
/// # Errors
/// Programming for a kind introduced by a later vocabulary.
pub fn drop_object(object: &SqlObject, schema: &str) -> Result<Option<String>> {
    Ok(match object.kind() {
        SqlObjectKind::Function(f) => Some(format!("DROP FUNCTION {} RESTRICT", signature(f))),
        SqlObjectKind::Trigger(t) => Some(format!(
            "DROP TRIGGER {} ON {}.{} RESTRICT",
            quote(t.name()),
            quote(schema),
            quote(t.model().table())
        )),
        SqlObjectKind::ForeignKey(f) => Some(format!(
            "ALTER TABLE {}.{} DROP CONSTRAINT {} RESTRICT",
            quote(schema),
            quote(f.model().table()),
            quote(f.name())
        )),
        SqlObjectKind::Condition(_)
        | SqlObjectKind::TableGrant(_)
        | SqlObjectKind::TableOptions(_) => None,
        _ => return Err(Error::programming("SQL object removal kind is unsupported")),
    })
}
