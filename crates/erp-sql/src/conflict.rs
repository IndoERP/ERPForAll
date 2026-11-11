//! The diagnostic behind "existing records conflict" (design §10): the pairs of existing
//! rows that stop a business key, an exclusion or a temporal key from being added.
//!
//! It runs on the failure path only, once, and returns ids only: PostgreSQL's detail line quotes
//! the rows' values, and values never reach a message (design §11.3).

use std::time::Duration;

use crate::quote;

/// `statement_timeout` (and, where the diagnostic takes its own transaction, `lock_timeout`) of a
/// conflict diagnostic query, on every path that runs one: the table is large by definition, and
/// the diagnostic only decorates an error that is already decided.
pub const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);

/// The text of a `statement_timeout` / `lock_timeout` setting for `timeout`, in milliseconds.
pub fn timeout_setting(timeout: Duration) -> String {
    format!("{}ms", timeout.as_millis())
}

/// The most id pairs a conflict message names.
pub const MAX_CONFLICT_PAIRS: u32 = 20;

/// How the rows of a pair must agree on the key columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyMatch {
    /// `=`: a NULL never agrees (exclusions, temporal keys, `NULLS DISTINCT` keys).
    Equal,
    /// `IS NOT DISTINCT FROM`: NULLs agree (`NULLS NOT DISTINCT` keys).
    NullsAgree,
}

/// A range expression over a row's columns compiled by this crate
/// ([`crate::exclusion::compiled_period`], [`crate::exclusion::compiled_range`]): the only SQL
/// [`conflict_pairs_query`] accepts for the period. Only this crate can make one.
#[derive(Debug, Clone)]
pub struct CompiledPeriod(String);

impl CompiledPeriod {
    pub(crate) fn new(compiled: String) -> Self {
        Self(compiled)
    }

    /// The SQL text.
    pub fn as_sql(&self) -> &str {
        &self.0
    }
}

/// A constant filter compiled by [`crate::constant_filter::compile_filter`]: the only SQL
/// [`conflict_pairs_query`] accepts for the filter. Only this crate can make one.
#[derive(Debug, Clone)]
pub struct CompiledFilter(String);

impl CompiledFilter {
    pub(crate) fn new(compiled: String) -> Self {
        Self(compiled)
    }

    /// The SQL text.
    pub fn as_sql(&self) -> &str {
        &self.0
    }
}

/// `SELECT a.id, b.id …`: up to [`MAX_CONFLICT_PAIRS`] pairs (any pairs: no `ORDER BY` over the
/// join, so `LIMIT` stops it early) of rows of `table` (`a.id < b.id`)
/// that agree on every column of `columns` ([`KeyMatch`]) and, with `period` (a range
/// expression over the row's columns), whose periods overlap. `filter` (a compiled constant
/// predicate) keeps rows it does not match out of both sides, like the `WHERE` of a partial
/// constraint. Only compiled SQL is accepted for `period` and `filter` ([`CompiledPeriod`],
/// [`CompiledFilter`]); table and column names are quoted.
pub fn conflict_pairs_query(
    table: &str,
    columns: &[&str],
    key_match: KeyMatch,
    period: Option<&CompiledPeriod>,
    filter: Option<&CompiledFilter>,
) -> String {
    let (period, filter) = (
        period.map(CompiledPeriod::as_sql),
        filter.map(CompiledFilter::as_sql),
    );
    let mut select = vec!["id".to_owned()];
    select.extend(
        columns
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{} AS k{i}", quote(c))),
    );
    if let Some(period) = period {
        select.push(format!("{period} AS p"));
    }
    let side = format!(
        "SELECT {} FROM {}{}",
        select.join(", "),
        quote(table),
        filter.map_or_else(String::new, |f| format!(" WHERE ({f})"))
    );
    let mut on = vec!["a.id < b.id".to_owned()];
    let op = match key_match {
        KeyMatch::Equal => "=",
        KeyMatch::NullsAgree => "IS NOT DISTINCT FROM",
    };
    on.extend((0..columns.len()).map(|i| format!("a.k{i} {op} b.k{i}")));
    if period.is_some() {
        on.push("a.p && b.p".to_owned());
    }
    format!(
        "SELECT a.id, b.id FROM ({side}) a JOIN ({side}) b ON {} LIMIT {}",
        on.join(" AND "),
        MAX_CONFLICT_PAIRS
    )
}

/// The diagnostic of a business key: one aggregate pass, `SELECT min(id), max(id) … GROUP BY <key
/// columns> HAVING count(*) > 1 LIMIT 20`, one `(first id, last id)` pair per group of rows that
/// share the key. With [`KeyMatch::Equal`] (a `NULLS DISTINCT` key) rows with a NULL part are left
/// out, as the constraint never compares them; `GROUP BY` puts NULLs together, like
/// [`KeyMatch::NullsAgree`].
pub fn key_conflict_query(table: &str, columns: &[&str], key_match: KeyMatch) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote(c)).collect();
    let filter = match key_match {
        KeyMatch::Equal => format!(
            " WHERE {}",
            cols.iter()
                .map(|c| format!("{c} IS NOT NULL"))
                .collect::<Vec<_>>()
                .join(" AND ")
        ),
        KeyMatch::NullsAgree => String::new(),
    };
    format!(
        "SELECT min(id), max(id) FROM {}{filter} GROUP BY {} HAVING count(*) > 1 LIMIT {}",
        quote(table),
        cols.join(", "),
        MAX_CONFLICT_PAIRS
    )
}

/// `(1, 2), (3, 4)`: the pairs as a message lists them.
pub(crate) fn format_pairs(pairs: &[(i64, i64)]) -> String {
    pairs
        .iter()
        .map(|(a, b)| format!("({a}, {b})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a conflict message names: a business key, a temporal key (both "key") or an exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictObject {
    /// A business key or a `WITHOUT OVERLAPS` key: `key '<name>'`.
    Key,
    /// An exclusion constraint: `exclusion '<name>'`.
    Exclusion,
}

impl ConflictObject {
    /// `key '<name>'` or `exclusion '<name>'`.
    fn phrase(self, name: &str) -> String {
        match self {
            Self::Key => format!("key '{name}'"),
            Self::Exclusion => format!("exclusion '{name}'"),
        }
    }
}

/// `cannot add <object> '<name>' to '<model>': existing records conflict, e.g. <pairs>` (sorted),
/// or [`conflict_message_unlisted`] when `pairs` is empty (the rows changed since the failure).
pub fn conflict_message(
    object: ConflictObject,
    name: &str,
    model: &str,
    pairs: &[(i64, i64)],
) -> String {
    if pairs.is_empty() {
        return conflict_message_unlisted(object, name, model);
    }
    let mut sorted = pairs.to_vec();
    sorted.sort_unstable();
    format!(
        "cannot add {} to '{model}': existing records conflict, e.g. {}",
        object.phrase(name),
        format_pairs(&sorted)
    )
}

/// The one message when no pair can be named (the diagnostic failed, timed out or found none):
/// the conflict is still the error.
pub fn conflict_message_unlisted(object: ConflictObject, name: &str, model: &str) -> String {
    format!(
        "cannot add {} to '{model}': existing records conflict (no pair could be listed: the \
         diagnostic query failed, timed out or found none)",
        object.phrase(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pairs_query() {
        assert_eq!(
            conflict_pairs_query("t", &["a", "b"], KeyMatch::NullsAgree, None, None),
            "SELECT a.id, b.id FROM (SELECT id, \"a\" AS k0, \"b\" AS k1 FROM \"t\") a JOIN \
             (SELECT id, \"a\" AS k0, \"b\" AS k1 FROM \"t\") b ON a.id < b.id AND a.k0 IS NOT \
             DISTINCT FROM b.k0 AND a.k1 IS NOT DISTINCT FROM b.k1 LIMIT 20"
        );
    }

    #[test]
    fn key_diagnostic_is_one_aggregate_pass() {
        assert_eq!(
            key_conflict_query("t", &["a", "b"], KeyMatch::NullsAgree),
            "SELECT min(id), max(id) FROM \"t\" GROUP BY \"a\", \"b\" HAVING count(*) > 1 LIMIT 20"
        );
        assert_eq!(
            key_conflict_query("t", &["a", "b"], KeyMatch::Equal),
            "SELECT min(id), max(id) FROM \"t\" WHERE \"a\" IS NOT NULL AND \"b\" IS NOT NULL \
             GROUP BY \"a\", \"b\" HAVING count(*) > 1 LIMIT 20"
        );
    }

    #[test]
    fn period_pairs_query_with_filter() {
        let sql = conflict_pairs_query(
            "t",
            &["r"],
            KeyMatch::Equal,
            Some(&CompiledPeriod::new(
                "tsrange(\"s\", \"e\", '[)')".to_owned(),
            )),
            Some(&CompiledFilter::new("\"r\" <> 0".to_owned())),
        );
        assert!(sql.contains(
            "SELECT id, \"r\" AS k0, tsrange(\"s\", \"e\", '[)') AS p FROM \"t\" WHERE (\"r\" <> 0)"
        ));
        assert!(sql.contains("ON a.id < b.id AND a.k0 = b.k0 AND a.p && b.p LIMIT"));
    }

    #[test]
    fn messages() {
        use ConflictObject::{Exclusion, Key};
        assert_eq!(
            conflict_message(Key, "_k", "m.x", &[(3, 4), (1, 2)]),
            "cannot add key '_k' to 'm.x': existing records conflict, e.g. (1, 2), (3, 4)"
        );
        assert_eq!(
            conflict_message(Exclusion, "_e", "m.x", &[]),
            conflict_message_unlisted(Exclusion, "_e", "m.x")
        );
        assert_eq!(
            conflict_message_unlisted(Key, "_k", "m.x"),
            "cannot add key '_k' to 'm.x': existing records conflict (no pair could be listed: \
             the diagnostic query failed, timed out or found none)"
        );
    }
}
