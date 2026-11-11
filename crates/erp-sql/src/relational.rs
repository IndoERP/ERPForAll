//! Statements of relational fields (REL §4.6–4.7, CRUD §4.6): many2many relation rows, record
//! deletion, hierarchy expansion. Every statement binds arrays (`int8[]`), so one prepared
//! statement serves any number of rows.

use std::any::Any;
use std::sync::Arc;

use erp_domain::{ExternalValue, Opaque, OpaqueKind, PrincipalParam, Value};
use erp_registry::{FieldId, ModelId};

use crate::{literal, quote};

/// A `child_of` / `parent_of` condition value: the records of `model` below (`descendants`) or
/// above the start records through the many2one `parent`, compiled as a sub-query of the
/// statement (`f IN (…)`), so the hierarchy never travels through the application (DOM §4.8).
/// It shows in messages as the operator and its start (`child_of[4, user.partner_id.id]`).
pub struct HierarchyQuery {
    /// The model of the hierarchy.
    pub model: ModelId,
    /// Its parent many2one (a column).
    pub parent: FieldId,
    /// `child_of` (true) or `parent_of`.
    pub descendants: bool,
    /// The start records.
    pub start: HierarchyStart,
}

impl std::fmt::Debug for HierarchyQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let op = if self.descendants {
            "child_of"
        } else {
            "parent_of"
        };
        let items: Vec<String> = match &self.start {
            HierarchyStart::Ids(ids) => ids.iter().map(i64::to_string).collect(),
            HierarchyStart::Paths(paths) => paths.iter().map(|p| format!("path {p}")).collect(),
            HierarchyStart::Selected { ids, params, .. } => ids
                .iter()
                .map(i64::to_string)
                .chain(params.iter().map(PrincipalParam::symbol))
                .collect(),
        };
        write!(f, "{op}[{}]", items.join(", "))
    }
}

/// The start records of a [`HierarchyQuery`].
#[derive(Debug)]
pub enum HierarchyStart {
    /// Ids (one `int8[]` parameter), walked with a recursive query.
    Ids(Vec<i64>),
    /// With `_parent_store`, the start records' `parent_path`s (one `text[]` parameter): `child_of`
    /// selects the paths under them with an index range per path (CRUD §4.11).
    Paths(Vec<String>),
    /// Records selected in the statement: literal ids (one `int8[]` parameter
    /// when there are any) and principal parameters (`user.partner_id.id`, `company_ids`), whose
    /// values are known only where the statement runs. With `by_path` (`child_of` on a
    /// `_parent_store` hierarchy) the subtree is selected by an index range on the start records'
    /// paths, read in the same statement; otherwise the hierarchy is walked recursively.
    Selected {
        /// Literal start ids.
        ids: Vec<i64>,
        /// Principal parameters naming start records.
        params: Vec<PrincipalParam>,
        /// Select descendants by `parent_path` ranges.
        by_path: bool,
    },
}

impl ExternalValue for HierarchyQuery {
    fn kind(&self) -> OpaqueKind {
        OpaqueKind::Query
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl HierarchyQuery {
    /// The query as a condition value.
    pub fn into_value(self) -> Value {
        Value::Opaque(Opaque::new(Arc::new(self)))
    }

    /// The hierarchy query held by a condition value, if it is one.
    pub fn of(value: &Value) -> Option<&Self> {
        match value {
            Value::Opaque(o) => o.get().as_any().downcast_ref::<Self>(),
            _ => None,
        }
    }
}

/// Pairs per relation-table `INSERT`/`DELETE` (bounded statements and lock sets).
pub const PAIR_CHUNK: usize = 50_000;

/// Ids per `DELETE` of records.
pub const DELETE_CHUNK: usize = 10_000;

/// A many2many relation table and its columns.
#[derive(Debug, Clone, Copy)]
pub struct Relation<'a> {
    /// Table.
    pub table: &'a str,
    /// Column of the field's model.
    pub column1: &'a str,
    /// Column of the comodel.
    pub column2: &'a str,
}

/// The pairs of a many2many for the records in `$n` (`int8[]`), in the comodel's order, with the
/// comodel's columns `extra` (REL §4.7.5):
///
/// ```sql
/// SELECT "rel"."c1", "rel"."c2"[, "co"."x"…] FROM "co" JOIN "rel" ON ("rel"."c2" = "co"."id")<joins>
/// WHERE "rel"."c1" = ANY($n::int8[]) [AND <predicate>] ORDER BY <order>
/// ```
///
/// `predicate` (the field domain, with its parameters first) and `order` are compiled on the
/// comodel's table.
pub fn select_m2m(
    rel: Relation<'_>,
    comodel_table: &str,
    extra: &[&str],
    joins: &str,
    predicate: Option<&str>,
    order: &str,
    ids_param: usize,
) -> String {
    let (r, co) = (quote(rel.table), quote(comodel_table));
    let mut cols = vec![
        format!("{r}.{}", quote(rel.column1)),
        format!("{r}.{}", quote(rel.column2)),
    ];
    cols.extend(extra.iter().map(|c| format!("{co}.{}", quote(c))));
    let mut out = format!(
        "SELECT {} FROM {co} JOIN {r} ON ({r}.{} = {co}.{}){joins} WHERE {r}.{} = ANY(${ids_param}::int8[])",
        cols.join(", "),
        quote(rel.column2),
        quote("id"),
        quote(rel.column1),
    );
    if let Some(predicate) = predicate {
        out.push_str(" AND ");
        out.push_str(predicate);
    }
    if !order.is_empty() {
        out.push_str(" ORDER BY ");
        out.push_str(order);
    }
    out
}

/// Add pairs (`$1`, `$2`: `int8[]` of equal length), ignoring existing ones (REL §4.7.6). Rows are
/// inserted in `(column1, column2)` order so that concurrent writers take index locks in the same
/// order (deadlock avoidance); callers send at most [`PAIR_CHUNK`] pairs.
pub fn insert_pairs(rel: Relation<'_>) -> String {
    format!(
        "INSERT INTO {} ({}, {}) SELECT * FROM unnest($1::int8[], $2::int8[]) ORDER BY 1, 2 ON \
         CONFLICT DO NOTHING",
        quote(rel.table),
        quote(rel.column1),
        quote(rel.column2)
    )
}

/// Remove pairs (`$1`, `$2`: `int8[]` of equal length) (REL §4.7.6), in `(column1, column2)`
/// order; callers send at most [`PAIR_CHUNK`] pairs, sorted.
pub fn delete_pairs(rel: Relation<'_>) -> String {
    format!(
        "DELETE FROM {} WHERE ({}, {}) IN (SELECT * FROM unnest($1::int8[], $2::int8[]) ORDER BY \
         1, 2)",
        quote(rel.table),
        quote(rel.column1),
        quote(rel.column2)
    )
}

/// `SELECT "id" FROM "t" WHERE "column" = ANY($1::int8[])`: the rows pointing at `$1` (the rows a
/// deletion of `$1` sets null or cascades to).
pub fn ids_referencing(table: &str, column: &str) -> String {
    format!(
        "SELECT {id} FROM {t} WHERE {c} = ANY($1::int8[])",
        id = quote("id"),
        t = quote(table),
        c = quote(column)
    )
}

/// `SELECT DISTINCT "column" FROM "t" WHERE "id" = ANY($1::int8[]) AND "column" IS NOT NULL`:
/// the targets of a many2one for the records `$1`.
pub fn targets_of(table: &str, column: &str) -> String {
    format!(
        "SELECT DISTINCT {c} FROM {t} WHERE {id} = ANY($1::int8[]) AND {c} IS NOT NULL",
        id = quote("id"),
        t = quote(table),
        c = quote(column)
    )
}

/// `SELECT DISTINCT "column1" FROM "rel" WHERE "column2" = ANY($1::int8[])`: the records whose
/// many2many lists one of `$1`.
pub fn m2m_owners(rel: Relation<'_>) -> String {
    format!(
        "SELECT DISTINCT {c1} FROM {r} WHERE {c2} = ANY($1::int8[])",
        r = quote(rel.table),
        c1 = quote(rel.column1),
        c2 = quote(rel.column2)
    )
}

/// `DELETE FROM "t" WHERE "id" = ANY($1::int8[])` (CRUD §4.6).
pub fn delete_records(table: &str) -> String {
    format!(
        "DELETE FROM {} WHERE {} = ANY($1::int8[])",
        quote(table),
        quote("id")
    )
}

/// Delete the IDs bound as `$1::int8[]`, returning only IDs actually deleted.
///
/// Missing IDs produce no row. PostgreSQL does not guarantee the returned row order.
///
/// ```
/// let sql = erp_sql::relational::delete_records_returning("items");
/// assert_eq!(sql, "DELETE FROM \"items\" WHERE \"id\" = ANY($1::int8[]) RETURNING \"id\"");
/// ```
pub fn delete_records_returning(table: &str) -> String {
    format!("{} RETURNING {}", delete_records(table), quote("id"))
}

/// Every record below the records in `$1` through `parent` (the start ids included, existing or
/// not), cycles tolerated (DOM §4.8 `child_of` without `parent_path`).
pub fn descendants(table: &str, parent: &str) -> String {
    descendants_of(table, parent, "$1::int8[]")
}

/// [`descendants`] with the start ids in parameter `param` (`$n::int8[]`), for a sub-query.
pub fn descendants_of(table: &str, parent: &str, param: &str) -> String {
    let (t, p, id) = (quote(table), quote(parent), quote("id"));
    format!(
        "WITH RECURSIVE \"__h\"(\"id\") AS (SELECT unnest({param}) UNION SELECT {t}.{id} FROM \
         {t} JOIN \"__h\" ON {t}.{p} = \"__h\".\"id\") SELECT \"__h\".\"id\" FROM \"__h\""
    )
}

/// Every existing record of `$1` and the records above them through `parent`, cycles tolerated
/// (DOM §4.8 `parent_of` without `parent_path`).
pub fn ancestors(table: &str, parent: &str) -> String {
    ancestors_of(table, parent, "$1::int8[]")
}

/// [`ancestors`] with the start ids in parameter `param` (`$n::int8[]`), for a sub-query.
pub fn ancestors_of(table: &str, parent: &str, param: &str) -> String {
    let (t, p, id) = (quote(table), quote(parent), quote("id"));
    format!(
        "WITH RECURSIVE \"__h\"(\"id\") AS (SELECT {t}.{id} FROM {t} WHERE {t}.{id} = \
         ANY({param}) UNION SELECT {t}.{p} FROM {t} JOIN \"__h\" ON {t}.{id} = \"__h\".\"id\" \
         WHERE {t}.{p} IS NOT NULL) SELECT \"__h\".\"id\" FROM \"__h\""
    )
}

/// The parent paths of the records `$1` (`int8[]`), just created (CRUD §4.11): their parent's path
/// followed by their own id and `/`, returned for the cache. One statement for the batch:
///
/// - `__n`: the new records and their parents;
/// - `__l`: the parents outside the batch, locked `FOR SHARE` (a concurrent move of a parent
///   then fails this transaction instead of leaving a stale path under REPEATABLE READ);
/// - `__p`: recursive over the batch, records whose parent is outside it first (roots get
///   `"<id>/"`), then their children in the batch, so a parent created in the same batch gives
///   its path to its children whatever the order of the rows. A parent without a path gives
///   `NULL`, which the caller reports as an error (never a silent root).
///
/// ```sql
/// WITH RECURSIVE "__n" AS (SELECT "id", "parent_id" FROM "t" WHERE "id" = ANY($1::int8[])),
/// "__l" AS (SELECT "id", "parent_path" FROM "t" WHERE "id" IN (SELECT "parent_id" FROM "__n")
///   AND "id" <> ALL($1::int8[]) FOR SHARE),
/// "__p"("id", "path") AS (
///   SELECT "n"."id", CASE WHEN "n"."parent_id" IS NULL THEN concat("n"."id", '/')
///     WHEN "l"."parent_path" IS NULL THEN NULL ELSE concat("l"."parent_path", "n"."id", '/') END
///   FROM "__n" AS "n" LEFT JOIN "__l" AS "l" ON "l"."id" = "n"."parent_id"
///   WHERE "n"."parent_id" IS NULL OR "n"."parent_id" NOT IN (SELECT "id" FROM "__n")
///   UNION ALL
///   SELECT "n"."id", concat("__p"."path", "n"."id", '/') FROM "__n" AS "n"
///   JOIN "__p" ON "n"."parent_id" = "__p"."id")
/// UPDATE "t" SET "parent_path" = "__p"."path" FROM "__p" WHERE "t"."id" = "__p"."id"
/// RETURNING "t"."id", "t"."parent_path"
/// ```
///
/// Rows of the batch in a cycle among themselves are not reached and not returned; the caller
/// reports them.
pub fn parent_paths_of_new(table: &str, parent: &str) -> String {
    let (t, p, id, path) = (
        quote(table),
        quote(parent),
        quote("id"),
        quote("parent_path"),
    );
    format!(
        "WITH RECURSIVE \"__n\" AS (SELECT {id}, {p} FROM {t} WHERE {id} = ANY($1::int8[])), \
         \"__l\" AS (SELECT {id}, {path} FROM {t} WHERE {id} IN (SELECT {p} FROM \"__n\") AND {id} \
         <> ALL($1::int8[]) FOR SHARE), \"__p\"({id}, \"path\") AS (SELECT \"n\".{id}, CASE WHEN \
         \"n\".{p} IS NULL THEN concat(\"n\".{id}, '/') WHEN \"l\".{path} IS NULL THEN NULL ELSE \
         concat(\"l\".{path}, \"n\".{id}, '/') END FROM \"__n\" AS \"n\" LEFT JOIN \"__l\" AS \
         \"l\" ON \"l\".{id} = \"n\".{p} WHERE \"n\".{p} IS NULL OR \"n\".{p} NOT IN (SELECT {id} \
         FROM \"__n\") UNION ALL SELECT \"n\".{id}, concat(\"__p\".\"path\", \"n\".{id}, '/') FROM \
         \"__n\" AS \"n\" JOIN \"__p\" ON \"n\".{p} = \"__p\".{id}) UPDATE {t} SET {path} = \
         \"__p\".\"path\" FROM \"__p\" WHERE {t}.{id} = \"__p\".{id} RETURNING {t}.{id}, {t}.{path}"
    )
}

/// Lock the records `$1` (`int8[]`, ascending) for a parent-store move and read their paths:
/// under `REPEATABLE READ`, a record moved by a concurrent transaction since this one's snapshot
/// makes this statement fail with a serialization error instead of letting a stale path through.
pub fn lock_parent_paths(table: &str) -> String {
    let (t, id, path) = (quote(table), quote("id"), quote("parent_path"));
    format!("SELECT {id}, {path} FROM {t} WHERE {id} = ANY($1::int8[]) ORDER BY {id} FOR UPDATE")
}

/// `pg_advisory_xact_lock` on a key derived from `$1` (text): structure changes of one parent
/// store are serialized for the rest of the transaction.
pub const PARENT_STORE_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtext($1::text))";

/// The start records of a hierarchy selected in the statement: `start` is a condition on
/// `"__s"."id"` (the literal ids and the principal parameters, ORed).
fn start_records(table: &str, start: &str) -> String {
    let (t, id) = (quote(table), quote("id"));
    format!("SELECT \"__s\".{id} FROM {t} AS \"__s\" WHERE {start}")
}

/// [`descendants_of`] from the start records `start` (a condition on `"__s"."id"`, see
/// [`HierarchyStart::Selected`]): the existing start records and every record below them.
pub fn descendants_from(table: &str, parent: &str, start: &str) -> String {
    let (t, p, id) = (quote(table), quote(parent), quote("id"));
    let first = start_records(table, start);
    format!(
        "WITH RECURSIVE \"__h\"(\"id\") AS ({first} UNION SELECT {t}.{id} FROM {t} JOIN \"__h\" \
         ON {t}.{p} = \"__h\".\"id\") SELECT \"__h\".\"id\" FROM \"__h\""
    )
}

/// [`ancestors_of`] from the start records `start` (a condition on `"__s"."id"`): the existing
/// start records and every record above them.
pub fn ancestors_from(table: &str, parent: &str, start: &str) -> String {
    let (t, p, id) = (quote(table), quote(parent), quote("id"));
    let first = start_records(table, start);
    format!(
        "WITH RECURSIVE \"__h\"(\"id\") AS ({first} UNION SELECT {t}.{p} FROM {t} JOIN \"__h\" \
         ON {t}.{id} = \"__h\".\"id\" WHERE {t}.{p} IS NOT NULL) SELECT \"__h\".\"id\" FROM \
         \"__h\""
    )
}

/// [`subtree_by_paths`] under the start records `start` (a condition on `"__s"."id"`): their
/// paths are read in the same statement, then one index range per path, as for literal paths.
pub fn subtree_from(table: &str, start: &str) -> String {
    let (t, id, path) = (quote(table), quote("id"), quote("parent_path"));
    format!(
        "SELECT \"c\".{id} FROM {t} AS \"__s\" JOIN {t} AS \"c\" ON \"c\".{path} COLLATE \"C\" >= \
         \"__s\".{path} COLLATE \"C\" AND \"c\".{path} COLLATE \"C\" < (left(\"__s\".{path}, -1) || \
         '0') COLLATE \"C\" WHERE {start}"
    )
}

/// The records of `table` whose path lies under one of the paths `param` (`text[]`), by an
/// index range on the byte order of the paths per path (`'1/3/'` ≤ path < `'1/30'`), for
/// `child_of` with `_parent_store`.
pub fn subtree_by_paths(table: &str, param: &str) -> String {
    let (t, id, path) = (quote(table), quote("id"), quote("parent_path"));
    format!(
        "SELECT \"c\".{id} FROM unnest({param}) AS \"__r\"(\"p\") JOIN {t} AS \"c\" ON \
         \"c\".{path} COLLATE \"C\" >= \"__r\".\"p\" COLLATE \"C\" AND \"c\".{path} COLLATE \"C\" < \
         (left(\"__r\".\"p\", -1) || '0') COLLATE \"C\""
    )
}

/// The index of a parent store's paths: byte order (`COLLATE "C"`), so prefix ranges and
/// `LIKE 'prefix%'` use it whatever the database collation.
pub fn create_parent_path_index(table: &str, name: &str) -> String {
    format!(
        "CREATE INDEX {} ON {} USING btree ({} COLLATE \"C\")",
        quote(name),
        quote(table),
        quote("parent_path")
    )
}

/// Move the records `$1` (`int8[]`) and their descendants under the record `$2` (`int8`, NULL for
/// the root level), as their `parent` already says (CRUD §4.11): each path keeps its part from the
/// moved record on, after the new parent's path. Descendants are selected with an index-friendly
/// range on the byte order of the paths (`'1/3/'` ≤ path < `'1/30'`: `'/'` sorts just before
/// `'0'`).
///
/// ```sql
/// UPDATE "t" AS "child" SET "parent_path" = concat((SELECT "p"."parent_path" FROM "t" AS "p"
///   WHERE "p"."id" = $2::int8), substr("child"."parent_path",
///   length("node"."parent_path") - length("node"."id"::text || '/') + 1))
/// FROM "t" AS "node" WHERE "node"."id" = ANY($1::int8[])
///   AND "child"."parent_path" COLLATE "C" >= "node"."parent_path" COLLATE "C"
///   AND "child"."parent_path" COLLATE "C" < (left("node"."parent_path", -1) || '0') COLLATE "C"
/// RETURNING "child"."id", "child"."parent_path"
/// ```
pub fn move_parent_paths(table: &str) -> String {
    let (t, id, path) = (quote(table), quote("id"), quote("parent_path"));
    format!(
        "UPDATE {t} AS \"child\" SET {path} = concat((SELECT \"p\".{path} FROM {t} AS \"p\" WHERE \
         \"p\".{id} = $2::int8), substr(\"child\".{path}, length(\"node\".{path}) - \
         length(\"node\".{id}::text || '/') + 1)) FROM {t} AS \"node\" WHERE \"node\".{id} = \
         ANY($1::int8[]) AND \"child\".{path} COLLATE \"C\" >= \"node\".{path} COLLATE \"C\" AND \
         \"child\".{path} COLLATE \"C\" < (left(\"node\".{path}, -1) || '0') COLLATE \"C\" \
         RETURNING \"child\".{id}, \"child\".{path}"
    )
}

/// Every parent path of a table, from the roots (`parent` empty) down, in one statement (a new
/// `parent_path` column on an existing table, CRUD §4.11). Rows not reachable from a root (a cycle
/// in existing data) keep an empty path.
pub fn compute_parent_paths(table: &str, parent: &str) -> String {
    let (t, p, id, path) = (
        quote(table),
        quote(parent),
        quote("id"),
        quote("parent_path"),
    );
    format!(
        "WITH RECURSIVE \"__p\"(\"id\", \"path\") AS (SELECT {id}, concat({id}, '/') FROM {t} \
         WHERE {p} IS NULL UNION ALL SELECT \"c\".{id}, concat(\"__p\".\"path\", \"c\".{id}, '/') \
         FROM {t} AS \"c\" JOIN \"__p\" ON \"c\".{p} = \"__p\".\"id\") UPDATE {t} SET {path} = \
         \"__p\".\"path\" FROM \"__p\" WHERE {t}.{id} = \"__p\".\"id\""
    )
}

pub fn create_relation_table(rel: Relation<'_>, table1: &str, table2: &str) -> Vec<String> {
    let (r, c1, c2) = (quote(rel.table), quote(rel.column1), quote(rel.column2));
    let index = quote(&relation_index_name(rel));
    vec![
        format!(
            "CREATE TABLE {r} ({c1} int8 NOT NULL, {c2} int8 NOT NULL, PRIMARY KEY({c1}, {c2}))"
        ),
        format!(
            "COMMENT ON TABLE {r} IS {}",
            literal(&format!("RELATION BETWEEN {table1} AND {table2}"))
        ),
        format!("CREATE INDEX {index} ON {r} ({c2}, {c1})"),
    ]
}

/// The name of a relation table's `(column2, column1)` index (PostgreSQL's own convention).
pub fn relation_index_name(rel: Relation<'_>) -> String {
    erp_registry::ident::relation_index_name(rel.table, rel.column2, rel.column1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_returning_quotes_identifiers_and_preserves_legacy_delete() {
        assert_eq!(
            delete_records_returning("order\"line"),
            "DELETE FROM \"order\"\"line\" WHERE \"id\" = ANY($1::int8[]) RETURNING \"id\""
        );
        assert_eq!(
            delete_records("order\"line"),
            "DELETE FROM \"order\"\"line\" WHERE \"id\" = ANY($1::int8[])"
        );
    }

    const REL: Relation<'static> = Relation {
        table: "a_b_rel",
        column1: "a_id",
        column2: "b_id",
    };

    #[test]
    fn relation_statements() {
        assert_eq!(
            insert_pairs(REL),
            "INSERT INTO \"a_b_rel\" (\"a_id\", \"b_id\") SELECT * FROM unnest($1::int8[], \
             $2::int8[]) ORDER BY 1, 2 ON CONFLICT DO NOTHING"
        );
        assert_eq!(
            delete_pairs(REL),
            "DELETE FROM \"a_b_rel\" WHERE (\"a_id\", \"b_id\") IN (SELECT * FROM \
             unnest($1::int8[], $2::int8[]) ORDER BY 1, 2)"
        );
        assert_eq!(
            ids_referencing("t", "p_id"),
            "SELECT \"id\" FROM \"t\" WHERE \"p_id\" = ANY($1::int8[])"
        );
        assert_eq!(
            targets_of("t", "p_id"),
            "SELECT DISTINCT \"p_id\" FROM \"t\" WHERE \"id\" = ANY($1::int8[]) AND \"p_id\" IS \
             NOT NULL"
        );
        assert_eq!(
            m2m_owners(REL),
            "SELECT DISTINCT \"a_id\" FROM \"a_b_rel\" WHERE \"b_id\" = ANY($1::int8[])"
        );
        assert_eq!(
            select_m2m(REL, "b", &["active"], "", None, "\"b\".\"id\"", 1),
            "SELECT \"a_b_rel\".\"a_id\", \"a_b_rel\".\"b_id\", \"b\".\"active\" FROM \"b\" JOIN \
             \"a_b_rel\" ON (\"a_b_rel\".\"b_id\" = \"b\".\"id\") WHERE \"a_b_rel\".\"a_id\" = \
             ANY($1::int8[]) ORDER BY \"b\".\"id\""
        );
        let ddl = create_relation_table(REL, "a", "b");
        assert_eq!(
            ddl[0],
            "CREATE TABLE \"a_b_rel\" (\"a_id\" int8 NOT NULL, \"b_id\" int8 NOT NULL, PRIMARY \
             KEY(\"a_id\", \"b_id\"))"
        );
        assert_eq!(
            ddl[1],
            "COMMENT ON TABLE \"a_b_rel\" IS 'RELATION BETWEEN a AND b'"
        );
        assert_eq!(
            ddl[2],
            "CREATE INDEX \"a_b_rel_b_id_a_id_idx\" ON \"a_b_rel\" (\"b_id\", \"a_id\")"
        );
    }
}
