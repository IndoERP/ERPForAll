//! Running a query AST (spec `QAST`): [`check`] validates a [`QueryAst`] against the
//! transaction's registry and an environment's access without fetching rows, and [`run`] checks
//! it the same way and runs it as that environment's principal.
//!
//! The environment you pass is the only principal involved: the AST carries none, so a query
//! saved by an administrator and run by a clerk runs with the clerk's model access, record rules,
//! field groups and company scope. Principal parameters in the domain
//! (`{"param": "uid"}`) are bound from that environment's principal by the same lowering the
//! access predicates use (compiled predicate parameters); the document never
//! holds a value of its own for them.
//!
//! `run` goes through `Records::search_read` and `Records::read_group`, so it is the live call:
//! the same statement, the same method chain (`#[method(override)]` of `_read_group` applies),
//! the same errors. `check` does not call the method chain: it runs the planning half of the
//! same code (`search_plan`, `read_group::plan`), which sends no data statement.
//!
//! ```no_run
//! # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
//! use erp_domain::query::{QueryAst, QueryLimits};
//! use erp_orm::query::{QueryResult, run};
//!
//! let ast = QueryAst::parse(
//!     br#"{"version": 1, "kind": "search_read", "model": "res.partner",
//!          "domain": [["name", "ilike", "acme"]], "fields": ["name"], "limit": 20}"#,
//!     &QueryLimits::default(),
//! )?;
//! let env = tx.env().clone();
//! if let QueryResult::Rows { rows, truncated } = run(tx, &env, &ast).await? {
//!     println!("{} rows, more: {truncated}", rows.len());
//! }
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;

use erp_domain::Value as DomainValue;
use erp_domain::query::{
    Aggregate, FieldOrder, GroupBy, GroupOrder, HavingCondition, NullsPos, OrderDir, QueryAst,
    QueryKind,
};
use erp_fields::{Error, Result, Value};

use crate::access::Operation;
#[cfg(feature = "test-support")]
use crate::read_group::Plan;
use crate::read_group::{self, ReadGroup};
use crate::records::Records;
use crate::search::SearchOptions;
use crate::{Env, Transaction};

/// What a query returned.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum QueryResult {
    /// The rows of a `search_read` query, as `Records::search_read` returns them.
    Rows {
        /// At most `limit` rows.
        rows: Vec<BTreeMap<String, Value>>,
        /// Whether more rows matched than the query's `limit` (found in the same statement).
        truncated: bool,
    },
    /// The groups of a `read_group` query.
    Groups {
        /// The spec names of the columns: the group-bys, then the aggregates, in the AST's order.
        columns: Vec<String>,
        /// At most `limit` rows of values, one per column, as `Records::read_group` returns them.
        rows: Vec<Vec<Value>>,
        /// Whether more groups matched than the query's `limit`.
        truncated: bool,
    },
}

/// Validate `ast` against the transaction's registry and `env`'s access, without fetching rows.
///
/// In order: the model exists and is not abstract; `env`'s principal may read it; every field the
/// query names exists and is readable by the principal (in `fields`, the domain, `order`,
/// group-bys and aggregates); operators, values and granularities fit the field types. For a
/// query of plain conditions no data statement is sent once the principal's access state is
/// current (the first call of a transaction may read it); `child_of` / `parent_of` conditions and
/// `access` conditions are resolved through the database, so `check` may send statements for
/// them. Nothing is remembered: `run` checks again.
///
/// Superuser mode is refused (`Access`), as in [`run`].
///
/// # Errors
///
/// `Validation` for an unknown model, an abstract model, and the ORM's own texts for unknown
/// fields, paths and granularities; `Access` with the texts and -024.
pub async fn check(tx: &mut Transaction, env: &Env, ast: &QueryAst) -> Result<()> {
    let records = target(tx, env, ast).await?;
    match ast.kind() {
        QueryKind::SearchRead { fields, order } => {
            let opts = search_options(ast, order, fields)?;
            records.search_plan(tx, ast.domain(), &opts).await?;
            Ok(())
        }
        QueryKind::ReadGroup { .. } => {
            let args = read_group_args(ast)?;
            read_group::plan(tx, &records, &args).await?;
            Ok(())
        }
        _ => Err(unsupported_kind()),
    }
}

/// [`check`], then run the query as `env`'s principal.
///
/// An environment in superuser mode (`sudo()`, the `System` principal) is refused with an
/// `Access` error: a stored query runs with the rights of an ordinary principal, and audited
/// elevated replay is for and.
///
/// Fetches `limit + 1` rows or groups in the one statement and reports `truncated` when the extra
/// one exists, so a caller always knows whether the result is partial.
///
/// # Errors
///
/// As [`check`], and the errors of `Records::search_read` / `Records::read_group`.
pub async fn run(tx: &mut Transaction, env: &Env, ast: &QueryAst) -> Result<QueryResult> {
    let records = target(tx, env, ast).await?;
    let limit = page_len(ast.limit())?;
    match ast.kind() {
        QueryKind::SearchRead { fields, order } => {
            let opts = search_options(ast, order, fields)?;
            let names: Vec<&str> = fields.iter().map(|f| f.as_str()).collect();
            let mut rows = records.search_read(tx, ast.domain(), &names, &opts).await?;
            let truncated = cut(&mut rows, limit);
            Ok(QueryResult::Rows { rows, truncated })
        }
        QueryKind::ReadGroup {
            groupby,
            aggregates,
            ..
        } => {
            let args = read_group_args(ast)?;
            let mut rows = records.read_group(tx, args).await?;
            let truncated = cut(&mut rows, limit);
            let columns = groupby
                .iter()
                .map(|g| g.spec_name())
                .chain(aggregates.iter().map(Aggregate::spec_name))
                .collect();
            Ok(QueryResult::Groups {
                columns,
                rows,
                truncated,
            })
        }
        _ => Err(unsupported_kind()),
    }
}

/// The statement `run` would send for `ast` as `env`'s principal, and its parameters (debug
/// text, values included), for the statement snapshots. Tests only: behind the
/// `test-support` feature, which only `erp-security`'s dev-dependencies enable. It runs the same
/// model, field and rule checks as [`run`].
///
/// # Errors
///
/// As [`check`]; and `Programming` when the query is known to match nothing and sends no
/// statement.
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub async fn __statement(
    tx: &mut Transaction,
    env: &Env,
    ast: &QueryAst,
) -> Result<(String, Vec<String>)> {
    let records = target(tx, env, ast).await?;
    match ast.kind() {
        QueryKind::SearchRead { fields, order } => {
            let opts = search_options(ast, order, fields)?;
            let (query, columns) = records.search_plan(tx, ast.domain(), &opts).await?;
            if query.is_empty() {
                return Err(no_statement());
            }
            let (sql, params) = query.statement(&columns)?;
            Ok((
                sql.to_string(),
                params.iter().map(|p| format!("{p:?}")).collect(),
            ))
        }
        QueryKind::ReadGroup { .. } => {
            let args = read_group_args(ast)?;
            match read_group::plan(tx, &records, &args).await? {
                Plan::Empty(_) => Err(no_statement()),
                Plan::Statement(prepared) => Ok((
                    prepared.sql.clone(),
                    prepared.params.iter().map(|p| format!("{p:?}")).collect(),
                )),
            }
        }
        _ => Err(unsupported_kind()),
    }
}

#[cfg(feature = "test-support")]
fn no_statement() -> Error {
    Error::programming("the query is known to match nothing: no statement would be sent")
}

fn unsupported_kind() -> Error {
    Error::programming("this query kind is not known to this version of the ORM")
}

/// The model of `ast` for `env` with the document's `active_test`: `env` is not in superuser mode,
/// the model exists, is not abstract, and the principal may read it. The model check comes before anything that names a
/// field or reads data.
async fn target(tx: &mut Transaction, env: &Env, ast: &QueryAst) -> Result<Records> {
    // a stored query must not run with the rights of superuser mode: nothing would be checked and
    // a favourite saved by anyone would read every record
    if env.su() {
        return Err(Error::access(
            "A query AST is not run in superuser mode: audited elevated replay is not supported \
             until a later release and a later release",
        ));
    }
    let env = match ast.context().active_test() {
        Some(flag) => env.with_context([("active_test", Value::Bool(flag))]),
        None => env.clone(),
    };
    let name = ast.model();
    let registry = env.registry();
    let Some(model_id) = registry.model_id(name) else {
        return Err(Error::validation(format!(
            "Unknown model '{name}' in query AST"
        )));
    };
    if registry.model_by_id(model_id).is_abstract() {
        return Err(Error::validation(format!(
            "Model '{name}' is abstract and has no records"
        )));
    }
    tx.prepare_access(&env).await?;
    tx.check_model(&env, model_id, Operation::Read)?;
    named_fields_exist(registry, model_id, ast)?;
    env.records(name)
}

/// The fields of `fields` and `order` of a `search_read` query exist: the ORM reports a name it
/// does not know as a `Programming` error (its callers are Rust code), and a stored query is
/// client input, so the AST says `Validation` before the ORM is asked.
/// Group-bys, aggregates and the domain are checked by the ORM as client input already.
fn named_fields_exist(
    registry: &erp_registry::Registry,
    model_id: erp_registry::ModelId,
    ast: &QueryAst,
) -> Result<()> {
    let QueryKind::SearchRead { fields, order } = ast.kind() else {
        return Ok(());
    };
    let spec = registry.model_by_id(model_id);
    let named = fields
        .iter()
        .map(|f| f.as_str())
        .chain(order.iter().map(|t| t.field().as_str()));
    for name in named {
        if spec.field(name).is_none() {
            return Err(Error::validation(format!(
                "Invalid field '{name}' on model '{}'",
                spec.name
            )));
        }
    }
    Ok(())
}

/// `limit + 1`: the extra row tells whether more matched.
fn probe(limit: u32) -> Result<u64> {
    u64::from(limit)
        .checked_add(1)
        .ok_or_else(|| Error::programming("the page size of a query overflows"))
}

fn page_len(limit: u32) -> Result<usize> {
    usize::try_from(limit).map_err(|_| Error::programming("the page size of a query overflows"))
}

/// Cut `rows` to `limit`; whether there were more.
fn cut<T>(rows: &mut Vec<T>, limit: usize) -> bool {
    let more = rows.len() > limit;
    rows.truncate(limit);
    more
}

fn direction(dir: OrderDir, nulls: Option<NullsPos>) -> String {
    let mut text = format!(" {}", dir.as_str());
    if let Some(nulls) = nulls {
        text.push_str(" nulls ");
        text.push_str(nulls.as_str());
    }
    text
}

fn search_options(
    ast: &QueryAst,
    order: &[FieldOrder],
    fields: &[erp_domain::query::FieldName],
) -> Result<SearchOptions> {
    let mut opts = SearchOptions::new()
        .limit(probe(ast.limit())?)
        .offset(u64::from(ast.offset()))
        .fetch_fields(fields.iter().map(|f| f.as_str()));
    if !order.is_empty() {
        // names are validated identifiers and the direction words come from enums
        let text: Vec<String> = order
            .iter()
            .map(|t| format!("{}{}", t.field(), direction(t.dir(), t.nulls())))
            .collect();
        opts = opts.order(text.join(", "));
    }
    Ok(opts)
}

fn read_group_args(ast: &QueryAst) -> Result<ReadGroup> {
    let QueryKind::ReadGroup {
        groupby,
        aggregates,
        having,
        order,
    } = ast.kind()
    else {
        return Err(unsupported_kind());
    };
    let mut args = ReadGroup::new(ast.domain().clone())
        .groupby(groupby.iter().map(|g| g.spec_name()))
        .aggregates(aggregates.iter().map(Aggregate::spec_name))
        .having(having_items(having)?)
        .offset(u64::from(ast.offset()))
        .limit(probe(ast.limit())?)
        .external(true)
        .check_empty(true);
    if !order.is_empty() {
        args = args.order(group_order(order, groupby));
    }
    Ok(args)
}

/// The order terms, then the group-bys the order does not mention, ascending: groups tied on the
/// aggregates keep a fixed order, so `offset` paging and the `truncated` probe are stable.
fn group_order(order: &[GroupOrder], groupby: &[GroupBy]) -> String {
    let mut terms: Vec<String> = order
        .iter()
        .map(|t| format!("{}{}", t.key(), direction(t.dir(), t.nulls())))
        .collect();
    for group in groupby {
        let name = group.spec_name();
        if !order.iter().any(|t| t.key() == name) {
            terms.push(format!("{name} asc"));
        }
    }
    terms.join(", ")
}

/// The `having` conditions as the domain-like list `_read_group` takes: ANDed, in prefix form.
fn having_items(having: &[HavingCondition]) -> Result<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();
    for _ in 1..having.len() {
        items.push(Value::str("&"));
    }
    for condition in having {
        items.push(Value::list([
            Value::str(condition.aggregate()),
            Value::str(condition.op().as_str()),
            dynamic_value(condition.value())?,
        ]));
    }
    Ok(items)
}

/// The dynamic value of a `having` operand (scalars and lists of scalars, as validated).
fn dynamic_value(value: &DomainValue) -> Result<Value> {
    Ok(match value {
        DomainValue::Bool(flag) => Value::Bool(*flag),
        DomainValue::Int(n) => Value::Int(*n),
        DomainValue::Float(f) => Value::Float(*f),
        DomainValue::Str(text) => Value::str(text),
        DomainValue::List(items) => Value::list(
            items
                .iter()
                .map(dynamic_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        other => {
            return Err(Error::programming(format!(
                "a having value of kind {:?} is not part of a query AST",
                other.kind()
            )));
        }
    })
}
