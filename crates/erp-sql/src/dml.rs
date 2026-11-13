//! Set-based data statements (§11): `INSERT … SELECT … FROM unnest(…)`,
//! `SELECT … WHERE id = ANY($1)` and `UPDATE … FROM unnest(…)`. Every statement binds one array
//! parameter per column, so one prepared statement serves any number of rows.
//!
//! Placeholders are cast without typmod (`$2::varchar[]`, never `varchar(3)[]`): an explicit cast to
//! `varchar(n)` would silently truncate, while assignment to the column raises the error.

use std::sync::Arc;

use erp_fields::ColumnType;
use erp_registry::{FieldId, ModelSpec, Registry};

use crate::{literal, quote};

/// Where a table's new ids come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdSource<'a> {
    /// The table's own identity column (`pg_get_serial_sequence('"t"', 'id')`).
    Identity,
    /// A named sequence shared by several tables (`id_sequence`).
    Named(&'a str),
}

impl IdSource<'_> {
    /// The SQL expression naming the sequence of `table`, as text.
    fn sequence_expr(self, table: &str) -> String {
        match self {
            Self::Identity => format!("pg_get_serial_sequence({}, 'id')", literal(&quote(table))),
            Self::Named(seq) => format!("{}::text", literal(&quote(seq))),
        }
    }
}

/// Reserve `$1` ids of `table` in ascending order, with the transaction timestamp (F6:
/// a multi-row create takes its ids first, so ids follow the input order whatever the column
/// sets):
///
/// ```sql
/// WITH "__g" AS MATERIALIZED (
///   SELECT "n" FROM generate_series(1, $1::int8) AS "__g"("n") ORDER BY "n"),
/// "__s" AS (SELECT pg_get_serial_sequence('"t"', 'id') AS "seq")
/// SELECT nextval((SELECT "seq" FROM "__s")), (now() AT TIME ZONE 'UTC') FROM "__g"
/// ```
///
/// The ordered rows are materialized first and `nextval` reads them in a plain scan: the
/// sequence is drawn in that order without relying on when the planner evaluates a volatile
/// target list relative to an `ORDER BY`.
///
/// With [`IdSource::Named`] the sequence is named directly (`'"seq"'::text`).
pub fn reserve_ids(table: &str, ids: IdSource<'_>) -> String {
    format!(
        "WITH \"__g\" AS MATERIALIZED (SELECT \"n\" FROM generate_series(1, $1::int8) AS \
         \"__g\"(\"n\") ORDER BY \"n\"), \"__s\" AS (SELECT {seq} AS \
         \"seq\") SELECT nextval((SELECT \"seq\" FROM \"__s\")), {now} FROM \"__g\"",
        seq = ids.sequence_expr(table),
        now = crate::NOW_UTC,
    )
}

/// `INSERT` of rows with reserved ids (`$1`, `int8[]`) and the given columns (`$2…`), plus fixed
/// expressions per row (audit timestamps); nothing is returned:
///
/// ```sql
/// INSERT INTO "t" ("id", "a", "create_date")
/// SELECT "__u"."id", "__u"."a", (now() AT TIME ZONE 'UTC')
/// FROM unnest($1::int8[], $2::varchar[]) AS "__u"("id", "a")
/// ```
pub fn insert_rows_with_ids(
    table: &str,
    columns: &[(&str, ColumnType)],
    exprs: &[(&str, &str)],
) -> String {
    let mut arrays = vec!["$1::int8[]".to_owned()];
    let mut names = vec![quote("id")];
    for (i, (c, ty)) in columns.iter().enumerate() {
        arrays.push(format!("${}::{}[]", i + 2, ty.udt_name()));
        names.push(quote(c));
    }
    let mut targets = names.clone();
    let mut values: Vec<String> = names.iter().map(|n| format!("\"__u\".{n}")).collect();
    for (c, e) in exprs {
        targets.push(quote(c));
        values.push((*e).to_owned());
    }
    format!(
        "INSERT INTO {t} ({targets}) SELECT {values} FROM unnest({arrays}) AS \"__u\"({names})",
        t = quote(table),
        targets = targets.join(", "),
        values = values.join(", "),
        arrays = arrays.join(", "),
        names = names.join(", "),
    )
}

/// `INSERT` of a batch of rows that all give the same columns (callers group rows by column set),
/// returning `(id, ordinal)` pairs so ids map back to input rows explicitly, never by position.
/// The ids follow the input order (F6 without a separate reservation when all rows share
/// one column set): the rows are materialized in ordinal order (`__r`), then `nextval` draws the
/// sequence while scanning them, with the sequence name looked up once (`__s`):
///
/// ```sql
/// WITH "__r" AS MATERIALIZED (
///   SELECT "__u".* FROM unnest($1::varchar[], $2::int4[]) WITH ORDINALITY
///   AS "__u"("a", "b", "__ord") ORDER BY "__u"."__ord"),
/// "__s" AS (SELECT pg_get_serial_sequence('"t"', 'id') AS "seq"),
/// "__v" AS MATERIALIZED (SELECT nextval((SELECT "seq" FROM "__s")) AS "id", "__r".* FROM "__r"),
/// "__ins" AS (INSERT INTO "t" ("id", "a", "b", "create_date")
///   SELECT "id", "a", "b", (now() AT TIME ZONE 'UTC') FROM "__v" ORDER BY "__ord")
/// SELECT "id", "__ord", (now() AT TIME ZONE 'UTC') FROM "__v" ORDER BY "__ord"
/// ```
///
/// The third column is the transaction timestamp, which audit timestamps use.
///
/// `columns` are bound as `$1…$n` arrays; `exprs` are fixed SQL expressions per row (audit
/// timestamps). With no columns, `$1` is the row count (`generate_series`). `ids` names the
/// sequence.
pub fn insert_rows(
    table: &str,
    ids: IdSource<'_>,
    columns: &[(&str, ColumnType)],
    exprs: &[(&str, &str)],
) -> String {
    insert_rows_with(table, ids, columns, exprs, None)
}

/// [`insert_rows`] with the record check of `create` fused: the INSERT
/// returns `(<predicate>) IS TRUE` for every row it inserts, the predicate reading the inserted
/// row through the table's name, its parameters numbered after the columns' arrays (or the row
/// count). The rows come back as `(id, ordinal, timestamp, allowed)`:
///
/// ```sql
/// …, "__ins" AS (INSERT INTO "t" (…) SELECT … FROM "__v" ORDER BY "__ord"
///   RETURNING "t"."id", (<predicate>) IS TRUE AS "__ok")
/// SELECT "__v"."id", "__v"."__ord", (now() AT TIME ZONE 'UTC'), "__ins"."__ok"
///   FROM "__v" JOIN "__ins" ON "__ins"."id" = "__v"."id" ORDER BY "__v"."__ord"
/// ```
///
/// The predicate must read only the inserted row's own columns and other tables: sub-queries of
/// the statement do not see the rows it inserts.
pub fn insert_rows_checked(
    table: &str,
    ids: IdSource<'_>,
    columns: &[(&str, ColumnType)],
    exprs: &[(&str, &str)],
    predicate: &str,
) -> String {
    insert_rows_with(table, ids, columns, exprs, Some(predicate))
}

fn insert_rows_with(
    table: &str,
    ids: IdSource<'_>,
    columns: &[(&str, ColumnType)],
    exprs: &[(&str, &str)],
    check: Option<&str>,
) -> String {
    let id = quote("id");
    let ord = quote("__ord");
    let source = if columns.is_empty() {
        format!("generate_series(1, $1::int8) AS \"__u\"({ord})")
    } else {
        let arrays: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, (_, ty))| format!("${}::{}[]", i + 1, ty.udt_name()))
            .collect();
        let mut names: Vec<String> = columns.iter().map(|(c, _)| quote(c)).collect();
        names.push(ord.clone());
        format!(
            "unnest({}) WITH ORDINALITY AS \"__u\"({})",
            arrays.join(", "),
            names.join(", ")
        )
    };
    let mut targets = vec![id.clone()];
    let mut values = vec![id.clone()];
    for (c, _) in columns {
        targets.push(quote(c));
        values.push(quote(c));
    }
    for (c, e) in exprs {
        targets.push(quote(c));
        values.push((*e).to_owned());
    }
    let t = quote(table);
    let now = crate::NOW_UTC;
    let (returning, result) = match check {
        None => (
            String::new(),
            format!("SELECT {id}, {ord}, {now} FROM \"__v\" ORDER BY {ord}"),
        ),
        Some(predicate) => (
            format!(" RETURNING {t}.{id}, ({predicate}) IS TRUE AS \"__ok\""),
            format!(
                "SELECT \"__v\".{id}, \"__v\".{ord}, {now}, \"__ins\".\"__ok\" FROM \"__v\" JOIN \
                 \"__ins\" ON \"__ins\".{id} = \"__v\".{id} ORDER BY \"__v\".{ord}"
            ),
        ),
    };
    format!(
        "WITH \"__r\" AS MATERIALIZED (SELECT \"__u\".* FROM {source} ORDER BY \"__u\".{ord}), \
         \"__s\" AS (SELECT {seq} AS \"seq\"), \"__v\" AS MATERIALIZED \
         (SELECT nextval((SELECT \"seq\" FROM \"__s\")) AS {id}, \"__r\".* FROM \"__r\"), \"__ins\" \
         AS (INSERT INTO {t} ({targets}) SELECT {values} FROM \"__v\" ORDER BY {ord}{returning}) \
         {result}",
        seq = ids.sequence_expr(table),
        targets = targets.join(", "),
        values = values.join(", "),
    )
}

/// `SELECT "id", cols… FROM t WHERE "id" = ANY($1)` (`$1` is an `int8[]` of real ids).
pub fn select_by_ids(table: &str, columns: &[&str]) -> String {
    let q_table = quote(table);
    let mut out = String::with_capacity(64 + q_table.len() + columns.len() * 20);
    out.push_str("SELECT \"id\"");
    for c in columns {
        out.push_str(", ");
        out.push_str(&quote(c));
    }
    out.push_str(" FROM ");
    out.push_str(&q_table);
    out.push_str(" WHERE \"id\" = ANY($1)");
    out
}

/// Statement text for `select_by_ids` built from `ModelSpec` and pre-quoted identifiers.
pub fn select_by_ids_spec(registry: &Registry, spec: &ModelSpec, fields: &[FieldId]) -> String {
    let mut out = String::with_capacity(64 + spec.quoted_table().len() + fields.len() * 20);
    out.push_str("SELECT \"id\"");
    for fid in fields {
        out.push_str(", ");
        let field = spec.field_by_id(*fid);
        match crate::company::load_column(registry, field, field.quoted_name()) {
            Some(item) => out.push_str(&item),
            None => out.push_str(field.quoted_name()),
        }
    }
    out.push_str(" FROM ");
    out.push_str(spec.quoted_table());
    out.push_str(" WHERE \"id\" = ANY($1)");
    out
}

/// Cached statement text for `select_by_ids` (PERF.SQL-TEXT). A cache hit is meant to
/// allocate nothing (by construction; not measured, DEFERRED_PERFORMANCE).
pub fn select_by_ids_cached(registry: &Registry, spec: &ModelSpec, fields: &[FieldId]) -> Arc<str> {
    registry
        .statement_cache_of(spec)
        .select_by_ids(fields, || select_by_ids_spec(registry, spec, fields))
}

/// `UPDATE t SET c = v.c, … FROM (SELECT * FROM unnest($1::int8[], $2::<type>[], …) AS
/// a("id", c, …) ORDER BY "id") AS v WHERE t."id" = v."id"`, plus `extra` assignments of fixed
/// expressions (e.g. `write_date`). The rows are joined in id order so concurrent updates of
/// overlapping sets take their row locks in the same order; callers also pass ids sorted and
/// chunked. The affected-row count tells whether every row still existed.
pub fn update_from_arrays(
    table: &str,
    columns: &[(&str, ColumnType)],
    extra: &[(&str, &str)],
) -> String {
    crate::version::update_from_arrays(
        table,
        columns,
        extra,
        &crate::version::VersionedUpdate::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_maps_ids_by_ordinality() {
        let sql = insert_rows(
            "t",
            IdSource::Identity,
            &[("a", ColumnType::Int4), ("b", ColumnType::Varchar(Some(3)))],
            &[("create_date", crate::NOW_UTC)],
        );
        assert_eq!(
            sql,
            "WITH \"__r\" AS MATERIALIZED (SELECT \"__u\".* FROM unnest($1::int4[], $2::varchar[]) \
             WITH ORDINALITY AS \"__u\"(\"a\", \"b\", \"__ord\") ORDER BY \"__u\".\"__ord\"), \"__s\" AS \
             (SELECT pg_get_serial_sequence('\"t\"', 'id') AS \"seq\"), \"__v\" AS MATERIALIZED (SELECT \
             nextval((SELECT \"seq\" FROM \"__s\")) AS \"id\", \"__r\".* FROM \"__r\"), \"__ins\" AS (INSERT INTO \"t\" (\"id\", \"a\", \"b\", \"create_date\") SELECT \"id\", \"a\", \"b\", \
             (now() AT TIME ZONE 'UTC') FROM \"__v\" ORDER BY \"__ord\") SELECT \"id\", \"__ord\", \
             (now() AT TIME ZONE 'UTC') FROM \"__v\" ORDER BY \"__ord\""
        );
    }

    #[test]
    fn reserved_ids_and_explicit_id_inserts() {
        assert_eq!(
            reserve_ids("t", IdSource::Identity),
            "WITH \"__g\" AS MATERIALIZED (SELECT \"n\" FROM generate_series(1, $1::int8) AS \
             \"__g\"(\"n\") ORDER BY \"n\"), \"__s\" AS (SELECT pg_get_serial_sequence('\"t\"', 'id') \
             AS \"seq\") SELECT nextval((SELECT \"seq\" FROM \"__s\")), (now() AT TIME ZONE 'UTC') \
             FROM \"__g\""
        );
        assert_eq!(
            insert_rows_with_ids(
                "t",
                &[("a", ColumnType::Int4)],
                &[("create_date", crate::NOW_UTC)]
            ),
            "INSERT INTO \"t\" (\"id\", \"a\", \"create_date\") SELECT \"__u\".\"id\", \
             \"__u\".\"a\", (now() AT TIME ZONE 'UTC') FROM unnest($1::int8[], $2::int4[]) AS \
             \"__u\"(\"id\", \"a\")"
        );
        assert_eq!(
            insert_rows_with_ids("t", &[], &[]),
            "INSERT INTO \"t\" (\"id\") SELECT \"__u\".\"id\" FROM unnest($1::int8[]) AS \
             \"__u\"(\"id\")"
        );
    }

    #[test]
    fn insert_without_columns_counts_rows() {
        let sql = insert_rows("t", IdSource::Identity, &[], &[]);
        assert!(
            sql.contains("FROM generate_series(1, $1::int8) AS \"__u\"(\"__ord\")"),
            "{sql}"
        );
        assert!(
            sql.contains("INSERT INTO \"t\" (\"id\") SELECT \"id\" FROM"),
            "{sql}"
        );
    }

    #[test]
    fn select_and_update_shapes() {
        assert_eq!(
            select_by_ids("t", &["name"]),
            "SELECT \"id\", \"name\" FROM \"t\" WHERE \"id\" = ANY($1)"
        );
        assert_eq!(
            update_from_arrays(
                "t",
                &[("name", ColumnType::Varchar(Some(3)))],
                &[("write_date", crate::NOW_UTC)]
            ),
            "UPDATE \"t\" SET \"name\" = \"__v\".\"name\", \"write_date\" = (now() AT TIME ZONE 'UTC') \
             FROM (SELECT * FROM unnest($1::int8[], $2::varchar[]) AS \"__a\"(\"id\", \"name\") \
             ORDER BY \"id\") AS \"__v\" WHERE \"t\".\"id\" = \"__v\".\"id\""
        );
    }

    /// a named sequence replaces the identity lookup in both id-drawing statements; the
    /// rest of the statement is the same.
    #[test]
    fn named_sequence_replaces_the_identity_lookup() {
        let named = reserve_ids("t", IdSource::Named("ir_actions_id_seq"));
        assert_eq!(
            named,
            reserve_ids("t", IdSource::Identity).replace(
                "pg_get_serial_sequence('\"t\"', 'id')",
                "'\"ir_actions_id_seq\"'::text"
            )
        );
        assert!(
            named.contains("(SELECT '\"ir_actions_id_seq\"'::text AS \"seq\")"),
            "{named}"
        );
        let cols = [("a", ColumnType::Int4)];
        let named = insert_rows("t", IdSource::Named("s"), &cols, &[]);
        assert_eq!(
            named,
            insert_rows("t", IdSource::Identity, &cols, &[])
                .replace("pg_get_serial_sequence('\"t\"', 'id')", "'\"s\"'::text")
        );
        let checked = insert_rows_checked("t", IdSource::Named("s"), &cols, &[], "TRUE");
        assert!(
            checked.contains("(SELECT '\"s\"'::text AS \"seq\")"),
            "{checked}"
        );
        assert!(!checked.contains("pg_get_serial_sequence"), "{checked}");
    }
}
