// SPDX-License-Identifier: MIT OR Apache-2.0
//! Validation of one raw declaration on its own: identifiers, admission bounds, the A3 type
//! vocabulary rules a type cannot express, the closed trigger variants and conditions. Rules that
//! look across declarations (references, collisions, cycles) are in [`super::graph`].

use erp_fields::Result;

use super::decl::{
    SqlBinding, SqlConditionDecl, SqlDep, SqlEvent, SqlFunctionDecl, SqlLanguage, SqlLevel,
    SqlObjectDecl, SqlObjectKindDecl, SqlResult, SqlSecurity, SqlTableGrantDecl, SqlTiming,
    SqlTriggerDecl,
};
use super::graph::{SqlTransition, SqlTriggerShape};
use super::{SqlObjectLimits, within};
use crate::ident::{MAX_IDENTIFIER_LEN, is_valid_pg_name};

/// Most allow-list columns of one catalog-derived function (contract §2).
pub(crate) const MAX_ALLOW_LIST: usize = 1024;

/// `(addon, key)` of the object an error names.
pub(crate) type Who<'a> = (&'a str, &'a str);

/// A local key or manifest entry: ASCII `[a-z][a-z0-9_]*`, at most 63 bytes.
pub(crate) fn check_key(who: Who<'_>) -> Result<()> {
    let key = who.1;
    within(who, "key bytes", key.len(), MAX_IDENTIFIER_LEN)?;
    let mut bytes = key.bytes();
    let head = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
    if !head || !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(err!(who, "key is not of the form [a-z][a-z0-9_]*"));
    }
    Ok(())
}

/// A physical identifier component: a valid PostgreSQL name of at most 63 bytes.
fn check_ident(who: Who<'_>, what: &str, value: &str) -> Result<()> {
    within(
        who,
        &format!("{what} bytes"),
        value.len(),
        MAX_IDENTIFIER_LEN,
    )?;
    if !is_valid_pg_name(value) {
        return Err(err!(who, "{what} '{value}' is not a valid identifier"));
    }
    Ok(())
}

const CORE_PREFIX: &str = "names starting with 'erp_' are reserved for core objects";
const CORE_SUFFIX: &str = "trigger names ending in '__row_version' are reserved for core";

/// A partial early refusal of names core uses (`erp_*`, `*__row_version` triggers). Core creates
/// other functions and triggers too; the installed-catalog collision refusal is B3's (contract §3
/// step 1, §4).
fn reserved(who: Who<'_>, name: &str, trigger: bool) -> Result<()> {
    let name = name.to_ascii_lowercase();
    match () {
        () if name.starts_with("erp_") => Err(err!(who, "{CORE_PREFIX}")),
        () if trigger && name.ends_with("__row_version") => Err(err!(who, "{CORE_SUFFIX}")),
        () => Ok(()),
    }
}

/// Validate `decl` on its own; a trigger's shape is mapped onto its closed variant.
pub(crate) fn check(
    decl: &SqlObjectDecl,
    limits: &SqlObjectLimits,
) -> Result<Option<SqlTriggerShape>> {
    let who = (decl.addon, decl.key);
    match &decl.kind {
        SqlObjectKindDecl::Function(f) => check_function(who, f, limits).map(|()| None),
        SqlObjectKindDecl::Trigger(t) => check_trigger(who, t).map(Some),
        SqlObjectKindDecl::Condition(c) => check_condition(who, c, limits).map(|()| None),
        SqlObjectKindDecl::TableGrant(g) => check_table_grant(who, g).map(|()| None),
    }
}

/// The first item listed twice in `items`, if any.
fn repeated<T: Ord + Copy>(items: &[T]) -> Option<T> {
    let mut sorted = items.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).find(|w| w[0] == w[1]).map(|w| w[0])
}

/// EXECUTE grants (A2): never to `schema_owner` (it owns invoker functions and is a member of the
/// definer), never to the owner of a definer function, never on a trigger function (a trigger
/// runs it without EXECUTE), each binding once.
fn check_execute(who: Who<'_>, f: &SqlFunctionDecl) -> Result<()> {
    if f.result == SqlResult::Trigger && !f.execute.is_empty() {
        return Err(err!(
            who,
            "a function returning trigger is run by its triggers; EXECUTE is not granted on it"
        ));
    }
    for binding in f.execute {
        let owner = match binding {
            SqlBinding::SchemaOwner => true,
            SqlBinding::Definer => f.security == SqlSecurity::Definer,
            SqlBinding::Maintenance | SqlBinding::Request => false,
        };
        if owner {
            return Err(err!(
                who,
                "binding '{binding}' owns the function; EXECUTE is not granted to it"
            ));
        }
    }
    if let Some(binding) = repeated(f.execute) {
        return Err(err!(
            who,
            "binding '{binding}' is granted EXECUTE more than once"
        ));
    }
    Ok(())
}

/// A table grant policy (A2, contract §5): to `request` or `definer` only (`schema_owner` owns the
/// table, `maintenance` executes declared entry points only), at least one operation, each once.
fn check_table_grant(who: Who<'_>, g: &SqlTableGrantDecl) -> Result<()> {
    match g.binding {
        SqlBinding::Request | SqlBinding::Definer => {}
        SqlBinding::SchemaOwner => {
            return Err(err!(
                who,
                "binding 'schema_owner' owns the table; it takes no grant"
            ));
        }
        SqlBinding::Maintenance => {
            return Err(err!(
                who,
                "binding 'maintenance' executes declared entry points only; it takes no table \
                 grant"
            ));
        }
    }
    if g.ops.is_empty() {
        return Err(err!(who, "a table grant lists no operation"));
    }
    if let Some(op) = repeated(g.ops) {
        return Err(err!(who, "operation {} is listed more than once", op.sql()));
    }
    Ok(())
}

fn check_function(who: Who<'_>, f: &SqlFunctionDecl, limits: &SqlObjectLimits) -> Result<()> {
    check_ident(who, "schema", f.schema)?;
    let schema = f.schema.to_ascii_lowercase();
    if schema.starts_with("pg_") || schema == "information_schema" {
        return Err(err!(who, "schema '{}' is reserved", f.schema));
    }
    check_ident(who, "function name", f.name)?;
    reserved(who, f.name, false)?;
    within(who, "arguments", f.args.len(), limits.arguments)?;
    // a catalog-derived body is checked as rendered for no columns here, and again as rendered
    // from the catalog at schema sync
    let empty_render = f.derived.map(|(_, render)| render(&[]));
    let body = empty_render.as_deref().unwrap_or(f.body);
    within(who, "body bytes", body.len(), limits.body_bytes)?;
    if body.trim().is_empty() {
        return Err(err!(who, "function body is empty"));
    }
    if let Some((columns, _)) = f.derived {
        within(
            who,
            "allow-list columns",
            columns.allow.len(),
            MAX_ALLOW_LIST,
        )?;
        let mut allow: Vec<&str> = columns.allow.to_vec();
        allow.sort_unstable();
        if let Some(pair) = allow.windows(2).find(|w| w[0] == w[1]) {
            return Err(err!(
                who,
                "allow-list field '{}' is listed more than once",
                pair[0]
            ));
        }
    }
    if f.result == SqlResult::Trigger && f.language == SqlLanguage::Sql {
        return Err(err!(
            who,
            "a function returning trigger must be written in PL/pgSQL"
        ));
    }
    if f.result == SqlResult::Trigger && !f.args.is_empty() {
        return Err(err!(who, "a function returning trigger takes no arguments"));
    }
    check_execute(who, f)?;
    within(who, "dependencies", f.deps.len(), limits.dependencies)?;
    // sorted copy of references only (bounded above): duplicates are adjacent
    let mut deps: Vec<&SqlDep> = f.deps.iter().collect();
    deps.sort_unstable();
    if let Some(pair) = deps.windows(2).find(|w| w[0] == w[1]) {
        let dep = describe_dep(pair[0]);
        return Err(err!(who, "dependency {dep} is listed more than once"));
    }
    Ok(())
}

/// `'<addon>/<key>'` or `model '<name>'`.
pub(crate) fn describe_dep(dep: &SqlDep) -> String {
    match dep {
        SqlDep::Object { addon, key } => format!("'{addon}/{key}'"),
        SqlDep::Model(model) => format!("model '{model}'"),
    }
}

/// Map the general trigger shape onto one of the four closed variants (contract §2), or name the
/// violated rule.
fn check_trigger(who: Who<'_>, t: &SqlTriggerDecl) -> Result<SqlTriggerShape> {
    check_ident(who, "trigger name", t.name)?;
    reserved(who, t.name, true)?;
    for alias in [t.old_table, t.new_table].into_iter().flatten() {
        check_ident(who, "transition alias", alias)?;
    }
    shape(t).map_err(|rule| err!(who, "{rule}"))
}

fn shape(t: &SqlTriggerDecl) -> std::result::Result<SqlTriggerShape, &'static str> {
    let event = match t.events {
        [] => return Err("a trigger needs exactly one event"),
        [event] => *event,
        _ => return Err("a trigger has exactly one event; declare one trigger per event"),
    };
    let transition = t.old_table.is_some() || t.new_table.is_some();
    if !t.update_of.is_empty() && transition {
        return Err("UPDATE OF column lists cannot be combined with transition tables");
    }
    if !t.update_of.is_empty() {
        return Err("UPDATE OF column lists are not supported");
    }
    if t.timing == SqlTiming::Before && transition {
        return Err("BEFORE triggers cannot have transition tables");
    }
    if t.constraint.is_some() && transition {
        return Err("constraint (deferrable) triggers cannot have transition tables");
    }
    let row = t.level == SqlLevel::Row;
    Ok(match (t.timing, event, t.constraint, row) {
        (SqlTiming::Before, SqlEvent::Truncate, None, false) => SqlTriggerShape::BeforeTruncate,
        (_, SqlEvent::Truncate, _, _) => {
            return Err(
                "TRUNCATE triggers are supported only as BEFORE TRUNCATE FOR EACH STATEMENT",
            );
        }
        (SqlTiming::Before, SqlEvent::Insert, None, true) => SqlTriggerShape::BeforeInsertRow,
        (SqlTiming::Before, ..) => {
            return Err(
                "BEFORE triggers are supported only as BEFORE INSERT FOR EACH ROW or \
                        BEFORE TRUNCATE FOR EACH STATEMENT",
            );
        }
        (SqlTiming::After, SqlEvent::Insert, Some(deferral), true) => {
            SqlTriggerShape::AfterInsertConstraintRow(deferral)
        }
        (SqlTiming::After, _, Some(_), _) => {
            return Err("constraint triggers are supported only as AFTER INSERT FOR EACH ROW");
        }
        (SqlTiming::After, _, None, true) => {
            return Err(
                "AFTER FOR EACH ROW triggers must be constraint triggers with an explicit \
                        deferral",
            );
        }
        (SqlTiming::After, _, None, false) => {
            SqlTriggerShape::AfterStatement(match (event, t.old_table, t.new_table) {
                (SqlEvent::Insert, None, Some(new_table)) => SqlTransition::Insert { new_table },
                (SqlEvent::Insert, ..) => {
                    return Err("AFTER INSERT FOR EACH STATEMENT needs NEW TABLE and no OLD TABLE");
                }
                (SqlEvent::Delete, Some(old_table), None) => SqlTransition::Delete { old_table },
                (SqlEvent::Delete, ..) => {
                    return Err("AFTER DELETE FOR EACH STATEMENT needs OLD TABLE and no NEW TABLE");
                }
                (SqlEvent::Update, Some(old), Some(new)) if old == new => {
                    return Err("transition aliases OLD TABLE and NEW TABLE must differ");
                }
                (SqlEvent::Update, Some(old_table), Some(new_table)) => SqlTransition::Update {
                    old_table,
                    new_table,
                },
                // TRUNCATE was answered above
                (SqlEvent::Update | SqlEvent::Truncate, ..) => {
                    return Err(
                        "AFTER UPDATE FOR EACH STATEMENT needs both OLD TABLE and NEW \
                                TABLE",
                    );
                }
            })
        }
    })
}

/// SQLSTATE in the `ERP` namespace (contract §6: standard and retry-control classes cannot be
/// registered), identifier, static message of 1 to the limit bytes.
fn check_condition(who: Who<'_>, c: &SqlConditionDecl, limits: &SqlObjectLimits) -> Result<()> {
    let state = c.sqlstate.as_bytes();
    let charset = |b: &u8| b.is_ascii_digit() || b.is_ascii_uppercase();
    if state.len() != 5 || !state.iter().all(charset) {
        return Err(err!(who, "SQLSTATE is not five characters of [0-9A-Z]"));
    }
    if !c.sqlstate.starts_with("ERP") {
        let state = c.sqlstate;
        return Err(err!(
            who,
            "SQLSTATE '{state}' is outside the ERP namespace (ERP00 to ERPZZ)"
        ));
    }
    check_ident(who, "condition identifier", c.identifier)?;
    let limit = limits.condition_message_bytes;
    within(who, "condition message bytes", c.message.len(), limit)?;
    if c.message.trim().is_empty() {
        return Err(err!(who, "condition message is empty"));
    }
    Ok(())
}

/// Body and metadata bytes of `decl` (every string it holds), or `None` on overflow.
pub(crate) fn decl_bytes(decl: &SqlObjectDecl) -> Option<usize> {
    let sum = |parts: &[usize]| parts.iter().try_fold(0usize, |a, b| a.checked_add(*b));
    let own = match &decl.kind {
        SqlObjectKindDecl::Function(f) => {
            let deps: Vec<usize> = f
                .deps
                .iter()
                .map(|d| match d {
                    SqlDep::Object { addon, key } => addon.len().saturating_add(key.len()),
                    SqlDep::Model(model) => model.len(),
                })
                .collect();
            let allow: Vec<usize> = f
                .derived
                .iter()
                .flat_map(|(c, _)| c.allow.iter().map(|a| a.len()).chain([c.model.len()]))
                .collect();
            sum(&[
                f.schema.len(),
                f.name.len(),
                f.body.len(),
                sum(&deps)?,
                sum(&allow)?,
            ])?
        }
        SqlObjectKindDecl::Trigger(t) => {
            let columns: Vec<usize> = t.update_of.iter().map(|c| c.len()).collect();
            sum(&[
                t.name.len(),
                t.model.len(),
                t.function.0.len(),
                t.function.1.len(),
                t.old_table.map_or(0, str::len),
                t.new_table.map_or(0, str::len),
                sum(&columns)?,
            ])?
        }
        SqlObjectKindDecl::Condition(c) => {
            sum(&[c.sqlstate.len(), c.identifier.len(), c.message.len()])?
        }
        SqlObjectKindDecl::TableGrant(g) => g.model.len(),
    };
    own.checked_add(decl.key.len())
}
