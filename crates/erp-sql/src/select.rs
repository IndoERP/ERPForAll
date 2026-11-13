//! `SELECT` statements over one model's table for searches (SPEC-DOM §4.11, §4.15).
//!
//! The predicate comes from [`crate::filter::compile_where`] with parameters `$1…$k`; limit and
//! offset are bound as the next parameters, so a statement's text depends only on the domain shape
//! and on which clauses are present (one prepared statement per shape).

use std::fmt::Write;
use std::sync::Arc;

use erp_registry::{FieldId, ModelSpec, Registry};

use crate::quote;

/// The clauses of a search statement.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchSql<'a> {
    /// Joins appended to the main table (` JOIN … ON (…)`, see `filter::Rendered::joins`).
    pub joins: Option<&'a str>,
    /// The predicate, or `None` for no `WHERE` clause.
    pub predicate: Option<&'a str>,
    /// The `ORDER BY` terms, or `None`.
    pub order: Option<&'a str>,
    /// Placeholder number of the `LIMIT` parameter, when there is a limit.
    pub limit: Option<usize>,
    /// Placeholder number of the `OFFSET` parameter, when the offset is not zero.
    pub offset: Option<usize>,
}

impl SearchSql<'_> {
    fn tail(&self, out: &mut String, with_order: bool) {
        if let Some(predicate) = self.predicate {
            out.push_str(" WHERE ");
            out.push_str(predicate);
        }
        if with_order && let Some(order) = self.order.filter(|o| !o.is_empty()) {
            out.push_str(" ORDER BY ");
            out.push_str(order);
        }
        if let Some(n) = self.limit {
            let _ = write!(out, " LIMIT ${n}::int8");
        }
        if let Some(n) = self.offset {
            let _ = write!(out, " OFFSET ${n}::int8");
        }
    }
}

/// `SELECT "t"."id"[, "t"."c"…] FROM "t" [WHERE …] [ORDER BY …] [LIMIT $n] [OFFSET $m]`.
pub fn select(table: &str, columns: &[&str], clauses: &SearchSql<'_>) -> String {
    let t = quote(table);
    let mut out = String::with_capacity(64 + t.len() * (columns.len() + 2) + columns.len() * 20);
    out.push_str("SELECT ");
    out.push_str(&t);
    out.push_str(".\"id\"");
    for c in columns {
        out.push_str(", ");
        out.push_str(&t);
        out.push('.');
        out.push_str(&quote(c));
    }
    out.push_str(" FROM ");
    out.push_str(&t);
    if let Some(joins) = clauses.joins {
        out.push_str(joins);
    }
    clauses.tail(&mut out, true);
    out
}

/// Statement text for `select` built from `ModelSpec` and pre-quoted identifiers.
pub fn select_spec(
    registry: &Registry,
    spec: &ModelSpec,
    columns: &[FieldId],
    clauses: &SearchSql<'_>,
) -> String {
    let mut out = String::with_capacity(
        64 + spec.quoted_table().len() * (columns.len() + 2) + columns.len() * 20,
    );
    out.push_str("SELECT ");
    out.push_str(spec.quoted_table());
    out.push_str(".\"id\"");
    for fid in columns {
        out.push_str(", ");
        let field = spec.field_by_id(*fid);
        let mut column = String::new();
        column.push_str(spec.quoted_table());
        column.push('.');
        column.push_str(field.quoted_name());
        match crate::company::load_column(registry, field, &column) {
            Some(item) => out.push_str(&item),
            None => out.push_str(&column),
        }
    }
    out.push_str(" FROM ");
    out.push_str(spec.quoted_table());
    if let Some(joins) = clauses.joins {
        out.push_str(joins);
    }
    clauses.tail(&mut out, true);
    out
}

/// Bounded selection catalog projection (amendment A).
///
/// Candidates are limited before cumulative octet accounting. Strings beyond either bound
/// are replaced by NULL on the server, before the driver receives them; column 2 is overflow.
/// The caller supplies authorized predicates and stored text fields, checked order ending in
/// the primary key, and positive bound parameter positions. NULL input stays NULL for refusal.
pub fn selection_catalog(
    spec: &ModelSpec,
    fields: [FieldId; 2],
    clauses: &SearchSql<'_>,
    byte_parameter: usize,
    pair_parameter: usize,
) -> String {
    let table = spec.quoted_table();
    let key = spec.field_by_id(fields[0]).quoted_name();
    let label = spec.field_by_id(fields[1]).quoted_name();
    let order = clauses.order.unwrap_or("");
    let order = if order.is_empty() {
        format!("{table}.\"id\"")
    } else {
        format!("{order}, {table}.\"id\"")
    };
    let mut sql = format!(
        "WITH candidates AS MATERIALIZED (SELECT {table}.{key} AS k, {table}.{label} AS l, row_number() OVER (ORDER BY {order}) AS pos FROM {table}"
    );
    if let Some(joins) = clauses.joins {
        sql.push_str(joins);
    }
    let clauses = SearchSql {
        order: Some(&order),
        ..*clauses
    };
    clauses.tail(&mut sql, true);
    sql.push_str(&format!("), sized AS (SELECT *, sum(octet_length(k)::bigint + octet_length(l)::bigint) OVER (ORDER BY pos ROWS UNBOUNDED PRECEDING) AS used FROM candidates) SELECT CASE WHEN k IS NOT NULL AND l IS NOT NULL AND used <= ${byte_parameter}::int8 AND pos <= ${pair_parameter}::int8 THEN k END, CASE WHEN k IS NOT NULL AND l IS NOT NULL AND used <= ${byte_parameter}::int8 AND pos <= ${pair_parameter}::int8 THEN l END, (used > ${byte_parameter}::int8 OR pos > ${pair_parameter}::int8) IS TRUE AS overflow FROM sized ORDER BY pos"));
    sql
}

/// Cached statement text for `select` (PERF.SQL-TEXT). A cache hit is meant to
/// allocate nothing (by construction; not measured, DEFERRED_PERFORMANCE).
pub fn select_cached(
    registry: &Registry,
    spec: &ModelSpec,
    columns: &[FieldId],
    clauses: &SearchSql<'_>,
) -> Arc<str> {
    let shape = erp_registry::SelectShape {
        joins: clauses.joins,
        predicate: clauses.predicate,
        order: clauses.order,
        limit: clauses.limit,
        offset: clauses.offset,
    };
    registry
        .statement_cache_of(spec)
        .select(columns, shape, || {
            select_spec(registry, spec, columns, clauses)
        })
}

/// `SELECT COUNT(*) FROM "t" [WHERE …]`, or with a limit or offset
/// `SELECT COUNT(*) FROM (SELECT FROM "t" WHERE … LIMIT $n) AS "__count"` (no `ORDER BY`).
pub fn count(table: &str, clauses: &SearchSql<'_>) -> String {
    let t = quote(table);
    let joins = clauses.joins.unwrap_or_default();
    if clauses.limit.is_none() && clauses.offset.is_none() {
        let mut out = format!("SELECT COUNT(*) FROM {t}{joins}");
        clauses.tail(&mut out, false);
        return out;
    }
    let mut inner = format!("SELECT FROM {t}{joins}");
    clauses.tail(&mut inner, false);
    format!("SELECT COUNT(*) FROM ({inner}) AS \"__count\"")
}

/// `SELECT "t"."id"[, cols…] FROM "t" WHERE "t"."id" = ANY($n::int8[]) [AND (<predicate>)]`: rows
/// of explicit records, restricted by an access predicate whose parameters come first.
pub fn select_ids_where(
    table: &str,
    columns: &[&str],
    joins: &str,
    predicate: Option<&str>,
    ids_param: usize,
) -> String {
    let t = quote(table);
    let mut out = String::with_capacity(
        64 + t.len() * (columns.len() + 2)
            + columns.len() * 20
            + joins.len()
            + predicate.map_or(0, |p| p.len() + 5),
    );
    out.push_str("SELECT ");
    out.push_str(&t);
    out.push_str(".\"id\"");
    for c in columns {
        out.push_str(", ");
        out.push_str(&t);
        out.push('.');
        out.push_str(&quote(c));
    }
    out.push_str(" FROM ");
    out.push_str(&t);
    out.push_str(joins);
    out.push_str(" WHERE ");
    out.push_str(&t);
    let _ = write!(out, ".\"id\" = ANY(${ids_param}::int8[])");
    if let Some(predicate) = predicate {
        out.push_str(" AND ");
        out.push_str(predicate);
    }
    out
}

/// Statement text for `select_ids_where_spec` built from `ModelSpec` and pre-quoted identifiers.
pub fn select_ids_where_spec(
    registry: &Registry,
    spec: &ModelSpec,
    columns: &[FieldId],
    joins: &str,
    predicate: Option<&str>,
    ids_param: usize,
) -> String {
    let mut out = String::with_capacity(
        64 + spec.quoted_table().len() * (columns.len() + 2)
            + columns.len() * 20
            + joins.len()
            + predicate.map_or(0, |p| p.len() + 5),
    );
    out.push_str("SELECT ");
    out.push_str(spec.quoted_table());
    out.push_str(".\"id\"");
    for fid in columns {
        out.push_str(", ");
        let field = spec.field_by_id(*fid);
        let mut column = String::new();
        column.push_str(spec.quoted_table());
        column.push('.');
        column.push_str(field.quoted_name());
        match crate::company::load_column(registry, field, &column) {
            Some(item) => out.push_str(&item),
            None => out.push_str(&column),
        }
    }
    out.push_str(" FROM ");
    out.push_str(spec.quoted_table());
    out.push_str(joins);
    out.push_str(" WHERE ");
    out.push_str(spec.quoted_table());
    let _ = write!(out, ".\"id\" = ANY(${ids_param}::int8[])");
    if let Some(predicate) = predicate {
        out.push_str(" AND ");
        out.push_str(predicate);
    }
    out
}

/// Cached statement text for `select_ids_where` (PERF.SQL-TEXT). A cache hit is meant to
/// allocate nothing (by construction; not measured, DEFERRED_PERFORMANCE).
pub fn select_ids_where_cached(
    registry: &Registry,
    spec: &ModelSpec,
    columns: &[FieldId],
    joins: &str,
    predicate: Option<&str>,
    ids_param: usize,
) -> Arc<str> {
    registry
        .statement_cache_of(spec)
        .select_ids_where(columns, joins, predicate, ids_param, || {
            select_ids_where_spec(registry, spec, columns, joins, predicate, ids_param)
        })
}

/// The id list of an ORM-produced id-list fetch as the predicate binds it.
const ID_LIST: &str = "= ANY($1::int8[])";
/// The same id list behind an `InitPlan` (5).
const ID_LIST_BLIND: &str = "= ANY((SELECT $1::int8[])::int8[])";

/// The predicate of an id-list fetch with its id list (`= ANY($1::int8[])`, the one literal)
/// read through a scalar sub-select, so the planner never sees the ids (5).
///
/// A plan made per call sees the array's elements: on a table whose statistics are older than its
/// rows (the bench templates: analyzed while empty, autovacuum off) each element counts as 0.5 %
/// of the rows, so 100 owners are estimated at half the table and a 100,000-row line table is
/// read sequentially. Behind the sub-select the array is a run-time value, as in the generic
/// plan: every plan of the statement is the one the generic plan has (the inverse index) whatever
/// the ids and the statistics, and `plan_cache_mode = auto` keeps the generic plan once it may.
///
/// `None` unless the predicate has that literal exactly once.
pub fn blind_id_list(predicate: &str) -> Option<String> {
    (predicate.matches(ID_LIST).count() == 1).then(|| predicate.replacen(ID_LIST, ID_LIST_BLIND, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_dom_051_052_statement_shapes() {
        let clauses = SearchSql {
            joins: None,
            predicate: Some("\"t\".\"name\" LIKE $1::text"),
            order: Some("\"t\".\"id\""),
            limit: Some(2),
            offset: None,
        };
        assert_eq!(
            select("t", &["name"], &clauses),
            "SELECT \"t\".\"id\", \"t\".\"name\" FROM \"t\" WHERE \"t\".\"name\" LIKE $1::text \
             ORDER BY \"t\".\"id\" LIMIT $2::int8"
        );
        assert_eq!(
            count("t", &clauses),
            "SELECT COUNT(*) FROM (SELECT FROM \"t\" WHERE \"t\".\"name\" LIKE $1::text LIMIT \
             $2::int8) AS \"__count\""
        );
        let plain = SearchSql {
            predicate: Some("x"),
            order: Some("o"),
            ..SearchSql::default()
        };
        assert_eq!(count("t", &plain), "SELECT COUNT(*) FROM \"t\" WHERE x");
        assert_eq!(
            select("t", &[], &SearchSql::default()),
            "SELECT \"t\".\"id\" FROM \"t\""
        );
        assert_eq!(
            select_ids_where("t", &["a"], "", Some("p"), 3),
            "SELECT \"t\".\"id\", \"t\".\"a\" FROM \"t\" WHERE \"t\".\"id\" = ANY($3::int8[]) AND p"
        );
    }

    /// 5: the id list moves behind a sub-select only when it is the one literal.
    #[test]
    fn the_id_list_moves_behind_a_sub_select() {
        assert_eq!(
            blind_id_list("\"t\".\"owner_id\" = ANY($1::int8[])").as_deref(),
            Some("\"t\".\"owner_id\" = ANY((SELECT $1::int8[])::int8[])")
        );
        assert_eq!(blind_id_list("\"t\".\"owner_id\" = ANY($2::int8[])"), None);
        assert_eq!(
            blind_id_list("\"a\" = ANY($1::int8[]) OR \"b\" = ANY($1::int8[])"),
            None
        );
        assert_eq!(blind_id_list("TRUE"), None);
    }
}
