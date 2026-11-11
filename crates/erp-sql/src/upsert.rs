//! Business keys and the upsert statement (–D2; design note
//! , §3.3, §3.5).
//!
//! - [`key_definition`]: the `UNIQUE NULLS NOT DISTINCT (…)` text of a business key constraint.
//! - [`upsert`]: one `INSERT … ON CONFLICT ON CONSTRAINT … DO UPDATE … WHERE <changed>` per batch
//!   that also returns, for every input row (by its ordinal), the id, whether it was inserted and
//!   the old and new row versions; rows the conflict `WHERE` left alone come from a second branch
//!   that reads them by key.
//! - [`existing_denied`]: the write-rule pre-check over the existing rows of a batch's keys
//!   ('s write shape: one `SELECT … WHERE (<pred>) IS NOT TRUE`), with whether the read
//!   rule lets each denied row through, so a row the caller cannot read is never named.
//!
//! Keys match NULL-safely, as the `NULLS NOT DISTINCT` arbiter does. A key column that cannot
//! hold NULL (a required field) is compared with `=` only, so the planner can use the key's index
//! and hash joins on it; a nullable one with `(a = b OR (a IS NULL AND b IS NULL))`.

use erp_fields::ColumnType;

use crate::quote;

/// `UNIQUE NULLS NOT DISTINCT ("a", "b")`, or `UNIQUE ("a", "b")` with `nulls_distinct`: the
/// definition of a business key constraint, also stored as its comment (CRUD §4.9).
pub fn key_definition(columns: &[&str], nulls_distinct: bool) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote(c)).collect();
    format!(
        "UNIQUE{} ({})",
        if nulls_distinct {
            ""
        } else {
            " NULLS NOT DISTINCT"
        },
        cols.join(", ")
    )
}

/// One key column: its name and whether it can hold NULL.
#[derive(Debug, Clone, Copy)]
pub struct KeyColumn<'a> {
    /// Column name.
    pub name: &'a str,
    /// The column accepts NULL (an optional field): matched NULL-safely.
    pub nullable: bool,
}

/// `<a>.<x> = <b>.<y> AND …` over the key columns, NULL-safe for nullable columns. `left` and
/// `right` give the alias and the column name to use on each side for every key column.
fn key_match(key: &[KeyColumn<'_>], left: &str, right: &str, left_cols: &[String]) -> String {
    key.iter()
        .zip(left_cols)
        .map(|(k, l)| {
            let a = format!("{}.{}", quote(left), quote(l));
            let b = format!("{}.{}", quote(right), quote(k.name));
            if k.nullable {
                format!("({a} = {b} OR ({a} IS NULL AND {b} IS NULL))")
            } else {
                format!("{a} = {b}")
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// How the conflict action sets a column of an existing row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetAction {
    /// `c = EXCLUDED.c` when it differs (`update`).
    Overwrite,
    /// `c = COALESCE(t.c, 0) + EXCLUDED.c` (`accumulate`: the row carries a delta).
    Accumulate,
    /// `c = GREATEST($floor, COALESCE(t.c, 0) + EXCLUDED.c)` (`accumulate_floor`); the floor is
    /// bound as parameter `param` of type `ty`.
    AccumulateFloor {
        /// Placeholder number of the floor.
        param: usize,
        /// Column type of the floor.
        ty: ColumnType,
    },
}

/// What the statement does with a row whose key exists.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ConflictAction<'a> {
    /// Get-or-create: `DO NOTHING`; existing rows come back unchanged.
    Nothing,
    /// `DO UPDATE SET …` for these columns, plus the audit columns copied from `EXCLUDED`, `WHERE`
    /// at least one of them changes.
    Update {
        /// `(column, action)` in order.
        sets: &'a [(&'a str, SetAction)],
        /// Columns copied from the proposed row when the row is updated (`write_uid`,
        /// `write_date`).
        audit: &'a [&'a str],
    },
}

/// The parts of an upsert statement ([`upsert`]).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Upsert<'a> {
    /// The model's table.
    pub table: &'a str,
    /// The arbiter: the business key constraint's name.
    pub constraint: &'a str,
    /// Input columns, bound as arrays `$1…$n` in this order (one element per input row).
    pub columns: &'a [(&'a str, ColumnType)],
    /// The key columns (each also in `columns`), in the constraint's order.
    pub key: &'a [KeyColumn<'a>],
    /// Columns inserted from a fixed expression for every row: audit columns, `row_version = 1`.
    pub insert_exprs: &'a [(&'a str, &'a str)],
    /// The conflict action.
    pub action: ConflictAction<'a>,
    /// The table has a row version: `row_version + 1` on update, old and new versions returned.
    pub row_version: bool,
    /// Columns whose new value is returned (the accumulated fields).
    pub returning: &'a [(&'a str, ColumnType)],
    /// Columns whose old value is returned for updated rows (write hooks, design §3.6); NULL for
    /// inserted and unchanged rows.
    pub returning_old: &'a [(&'a str, ColumnType)],
    /// Ordered NULL-safe OLD/NEW difference flags, appended after the result timestamp.
    pub returning_changed: &'a [&'a str],
}

impl<'a> Upsert<'a> {
    /// The statement parts; set the optional ones with the field-update syntax of the builder
    /// methods.
    pub fn new(
        table: &'a str,
        constraint: &'a str,
        columns: &'a [(&'a str, ColumnType)],
        key: &'a [KeyColumn<'a>],
        action: ConflictAction<'a>,
    ) -> Self {
        Self {
            table,
            constraint,
            columns,
            key,
            insert_exprs: &[],
            action,
            row_version: false,
            returning: &[],
            returning_old: &[],
            returning_changed: &[],
        }
    }

    /// Fixed insert expressions.
    #[must_use]
    pub fn insert_exprs(mut self, exprs: &'a [(&'a str, &'a str)]) -> Self {
        self.insert_exprs = exprs;
        self
    }

    /// The table has a row version.
    #[must_use]
    pub fn row_version(mut self, on: bool) -> Self {
        self.row_version = on;
        self
    }

    /// New values returned.
    #[must_use]
    pub fn returning(mut self, columns: &'a [(&'a str, ColumnType)]) -> Self {
        self.returning = columns;
        self
    }

    /// Request ordered database difference flags without returning business values.
    ///
    /// Flags follow the existing result timestamp; unchanged rows yield `false`.
    /// An empty projection preserves the legacy result shape.
    ///
    /// ```
    /// use erp_fields::ColumnType;
    /// use erp_sql::upsert::{Upsert, KeyColumn, ConflictAction, upsert};
    /// let columns = [("code", ColumnType::Int8)];
    /// let key = [KeyColumn { name: "code", nullable: false }];
    /// let request = Upsert::new("item", "item_key", &columns, &key,
    ///     ConflictAction::Nothing).returning_changed(&["code"]);
    /// assert!(upsert(&request).contains("IS DISTINCT FROM"));
    /// ```
    #[must_use]
    pub fn returning_changed(mut self, columns: &'a [&'a str]) -> Self {
        self.returning_changed = columns;
        self
    }

    /// Old values returned for updated rows.
    #[must_use]
    pub fn returning_old(mut self, columns: &'a [(&'a str, ColumnType)]) -> Self {
        self.returning_old = columns;
        self
    }
}

/// The upsert statement (design note §3.3). Result columns, one row per input row:
/// `ordinal` (1-based, `int8`), `id`, `inserted` (`bool`), `written` (`bool`: inserted or
/// updated; false for rows left unchanged), `old_version` and `version` (`int8`, NULL without a
/// row version; for unchanged rows both are the current version), the [`Upsert::returning`]
/// columns' new values, the [`Upsert::returning_old`] columns' old values, then the transaction
/// timestamp, then the [`Upsert::returning_changed`] flags (FALSE for unchanged rows).
///
/// ```sql
/// WITH "__in" AS MATERIALIZED (SELECT * FROM unnest($1::int8[], $2::float8[]) WITH ORDINALITY
///        AS "__i"("product_id", "quantity", "__ord")),
/// "__up" AS (INSERT INTO "t" AS "__t" ("product_id", "quantity", "row_version")
///   SELECT "product_id", "quantity", 1 FROM "__in" ORDER BY "__ord"
///   ON CONFLICT ON CONSTRAINT "t_key" DO UPDATE
///     SET "quantity" = COALESCE("__t"."quantity", 0) + EXCLUDED."quantity",
///         "row_version" = "__t"."row_version" + 1
///   WHERE EXCLUDED."quantity" <> 0
///   RETURNING WITH (OLD AS "__o", NEW AS "__n") "__n"."id" AS "__id",
///     "__n"."product_id" AS "__k0", "__o"."id" IS NULL AS "__inserted",
///     "__o"."row_version" AS "__old_version", "__n"."row_version" AS "__version",
///     "__n"."quantity" AS "__r0")
/// SELECT "__i"."__ord", "__u"."__id", "__u"."__inserted", TRUE, … FROM "__up" AS "__u"
///   JOIN "__in" AS "__i" ON <key>
/// UNION ALL
/// SELECT "__i"."__ord", "__t"."id", FALSE, FALSE, "__t"."row_version", "__t"."row_version", …
///   FROM "__in" AS "__i" JOIN "t" AS "__t" ON <key>
///  WHERE NOT EXISTS (SELECT FROM "__up" AS "__u" WHERE <key>)
/// ```
///
/// The second branch returns the rows the conflict `WHERE` locked but did not update (no-ops)
/// and, for `DO NOTHING`, every existing row; it reads them through the statement's snapshot,
/// which is their current version (under `REPEATABLE READ` a conflicting row the snapshot cannot
/// see makes the statement fail with `40001` first). The input must not repeat a key (the caller
/// merges duplicates): PostgreSQL refuses to update a row twice in one statement.
pub fn upsert(s: &Upsert<'_>) -> String {
    let t = quote(s.table);
    let alias = quote("__t");
    // input
    let arrays: Vec<String> = s
        .columns
        .iter()
        .enumerate()
        .map(|(i, (_, ty))| format!("${}::{}[]", i + 1, ty.udt_name()))
        .collect();
    let mut in_names: Vec<String> = s.columns.iter().map(|(c, _)| quote(c)).collect();
    in_names.push(quote("__ord"));
    let input = format!(
        "\"__in\" AS MATERIALIZED (SELECT * FROM unnest({}) WITH ORDINALITY AS \"__i\"({}))",
        arrays.join(", "),
        in_names.join(", ")
    );
    // insert
    let mut targets: Vec<String> = s.columns.iter().map(|(c, _)| quote(c)).collect();
    let mut values = targets.clone();
    for (c, e) in s.insert_exprs {
        targets.push(quote(c));
        values.push((*e).to_owned());
    }
    let conflict = match s.action {
        ConflictAction::Nothing => "DO NOTHING".to_owned(),
        ConflictAction::Update { sets, audit } => {
            let mut assigns: Vec<String> = Vec::new();
            let mut changed: Vec<String> = Vec::new();
            for (c, action) in sets {
                let col = quote(c);
                let current = format!("{alias}.{col}");
                let proposed = format!("EXCLUDED.{col}");
                match action {
                    SetAction::Overwrite => {
                        assigns.push(format!("{col} = {proposed}"));
                        changed.push(format!("{current} IS DISTINCT FROM {proposed}"));
                    }
                    SetAction::Accumulate => {
                        assigns.push(format!("{col} = COALESCE({current}, 0) + {proposed}"));
                        changed.push(format!("{proposed} <> 0"));
                    }
                    SetAction::AccumulateFloor { param, ty } => {
                        let value = format!(
                            "GREATEST(${param}::{}, COALESCE({current}, 0) + {proposed})",
                            ty.udt_name()
                        );
                        changed.push(format!("{value} IS DISTINCT FROM {current}"));
                        assigns.push(format!("{col} = {value}"));
                    }
                }
            }
            if s.row_version {
                let rv = quote("row_version");
                assigns.push(format!("{rv} = {alias}.{rv} + 1"));
            }
            for c in audit {
                let col = quote(c);
                assigns.push(format!("{col} = EXCLUDED.{col}"));
            }
            format!(
                "DO UPDATE SET {} WHERE {}",
                assigns.join(", "),
                changed.join(" OR ")
            )
        }
    };
    let rv = quote("row_version");
    let mut returning: Vec<String> = vec![format!("\"__n\".{} AS \"__id\"", quote("id"))];
    for (i, k) in s.key.iter().enumerate() {
        returning.push(format!("\"__n\".{} AS \"__k{i}\"", quote(k.name)));
    }
    returning.push(format!("\"__o\".{} IS NULL AS \"__inserted\"", quote("id")));
    if s.row_version {
        returning.push(format!("\"__o\".{rv} AS \"__old_version\""));
        returning.push(format!("\"__n\".{rv} AS \"__version\""));
    }
    for (i, (c, _)) in s.returning.iter().enumerate() {
        returning.push(format!("\"__n\".{} AS \"__r{i}\"", quote(c)));
    }
    for (i, (c, _)) in s.returning_old.iter().enumerate() {
        returning.push(format!("\"__o\".{} AS \"__ro{i}\"", quote(c)));
    }
    for (i, c) in s.returning_changed.iter().enumerate() {
        let column = quote(c);
        returning.push(format!(
            "\"__o\".{column} IS DISTINCT FROM \"__n\".{column} AS \"__changed{i}\""
        ));
    }
    let up = format!(
        "\"__up\" AS (INSERT INTO {t} AS {alias} ({targets}) SELECT {values} FROM \"__in\" ORDER BY \
         \"__ord\" ON CONFLICT ON CONSTRAINT {constraint} {conflict} RETURNING WITH (OLD AS \"__o\", \
         NEW AS \"__n\") {returning})",
        targets = targets.join(", "),
        values = values.join(", "),
        constraint = quote(s.constraint),
        returning = returning.join(", "),
    );
    // result branches
    let up_keys: Vec<String> = (0..s.key.len()).map(|i| format!("__k{i}")).collect();
    let in_keys: Vec<String> = s.key.iter().map(|k| k.name.to_owned()).collect();
    let up_match = key_match(s.key, "__u", "__i", &up_keys);
    let row_match = key_match(s.key, "__t", "__i", &in_keys);
    let mut first: Vec<String> = vec![
        "\"__i\".\"__ord\"".to_owned(),
        "\"__u\".\"__id\"".to_owned(),
        "\"__u\".\"__inserted\"".to_owned(),
        "TRUE".to_owned(),
    ];
    let mut second: Vec<String> = vec![
        "\"__i\".\"__ord\"".to_owned(),
        format!("{alias}.{}", quote("id")),
        "FALSE".to_owned(),
        "FALSE".to_owned(),
    ];
    if s.row_version {
        first.push("\"__u\".\"__old_version\"".to_owned());
        first.push("\"__u\".\"__version\"".to_owned());
        second.push(format!("{alias}.{rv}"));
        second.push(format!("{alias}.{rv}"));
    } else {
        first.extend(["NULL::int8".to_owned(), "NULL::int8".to_owned()]);
        second.extend(["NULL::int8".to_owned(), "NULL::int8".to_owned()]);
    }
    for (i, (c, _)) in s.returning.iter().enumerate() {
        first.push(format!("\"__u\".\"__r{i}\""));
        second.push(format!("{alias}.{}", quote(c)));
    }
    for (i, (_, ty)) in s.returning_old.iter().enumerate() {
        first.push(format!("\"__u\".\"__ro{i}\""));
        second.push(format!("NULL::{}", ty.udt_name()));
    }
    first.push(crate::NOW_UTC.to_owned());
    second.push(crate::NOW_UTC.to_owned());
    for i in 0..s.returning_changed.len() {
        first.push(format!("\"__u\".\"__changed{i}\""));
        second.push("FALSE".to_owned());
    }
    format!(
        "WITH {input}, {up} SELECT {first} FROM \"__up\" AS \"__u\" JOIN \"__in\" AS \"__i\" ON \
         {up_match} UNION ALL SELECT {second} FROM \"__in\" AS \"__i\" JOIN {t} AS {alias} ON \
         {row_match} WHERE NOT EXISTS (SELECT FROM \"__up\" AS \"__u\" WHERE {up_match})",
        first = first.join(", "),
        second = second.join(", "),
    )
}

/// The record rules of the pre-check ([`existing_denied`]): a matched row is selected when the
/// write predicate does not let it through or the read predicate does not let it through; the
/// read predicate also tells, per selected row, whether the caller may see it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PreCheckRules<'a> {
    write: Option<&'a str>,
    read: Option<&'a str>,
}

impl<'a> PreCheckRules<'a> {
    /// `write`: the compiled write predicate, `None` when it restricts nothing; `read`: the
    /// compiled read predicate, `None` when the caller may read every row of the model. With both
    /// `None` nothing is denied and the statement selects no row.
    pub const fn new(write: Option<&'a str>, read: Option<&'a str>) -> Self {
        Self { write, read }
    }
}

/// The pre-check of an upsert batch (design note §3.5 step 3, write row): the ids of
/// existing rows of the batch's keys that the write predicate does not let through **or** that the
/// read predicate does not let through (an upsert reports the id, version and accumulated values
/// of every row it matches, so the caller must be able to read it; review SEC-M1), each with
/// whether the read predicate lets it through (`readable`; the error for a row the caller cannot
/// read names no id, final review).
///
/// ```sql
/// SELECT "t"."id", (<read>) IS TRUE FROM "t" JOIN unnest($k+1::int8[], …) AS "__k"("product_id", …)
///   ON "t"."product_id" = "__k"."product_id" AND … <joins>
///  WHERE (<write>) IS NOT TRUE OR (<read>) IS NOT TRUE
/// ```
///
/// Both predicates and `joins` (the joins of both, merged) come from the domain compiler with
/// parameters `$1…$params` (the write predicate's first); the key arrays (types in `key_types`,
/// one per key column) follow.
pub fn existing_denied(
    table: &str,
    key: &[KeyColumn<'_>],
    key_types: &[ColumnType],
    joins: &str,
    rules: PreCheckRules<'_>,
    params: usize,
) -> String {
    let t = quote(table);
    let arrays: Vec<String> = key_types
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("${}::{}[]", params + i + 1, ty.udt_name()))
        .collect();
    let names: Vec<String> = key.iter().map(|k| quote(k.name)).collect();
    let cols: Vec<String> = key.iter().map(|k| k.name.to_owned()).collect();
    let on = key_match(key, table, "__k", &cols);
    let readable = match rules.read {
        Some(read) => format!("({read}) IS TRUE"),
        None => "TRUE".to_owned(),
    };
    let mut denied: Vec<String> = Vec::new();
    if let Some(write) = rules.write {
        denied.push(format!("({write}) IS NOT TRUE"));
    }
    if let Some(read) = rules.read {
        denied.push(format!("({read}) IS NOT TRUE"));
    }
    let denied = if denied.is_empty() {
        "FALSE".to_owned()
    } else {
        denied.join(" OR ")
    };
    format!(
        "SELECT {t}.{id}, {readable} FROM {t} JOIN unnest({arrays}) AS \"__k\"({names}) ON \
         {on}{joins} WHERE {denied}",
        id = quote("id"),
        arrays = arrays.join(", "),
        names = names.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_projection_preserves_legacy_layout_and_quotes_names() {
        let columns = [("key", ColumnType::Int8), ("odd\"name", ColumnType::Int4)];
        let key = [KeyColumn {
            name: "key",
            nullable: false,
        }];
        let sets = [("odd\"name", SetAction::Overwrite)];
        let base = Upsert::new(
            "t",
            "t_key",
            &columns,
            &key,
            ConflictAction::Update {
                sets: &sets,
                audit: &[],
            },
        )
        .row_version(true)
        .returning(&[("odd\"name", ColumnType::Int4)])
        .returning_old(&[("odd\"name", ColumnType::Int4)]);
        let legacy = upsert(&base);
        assert_eq!(legacy, upsert(&base.returning_changed(&[])));
        let sql = upsert(&base.returning_changed(&["odd\"name", "key"]));
        assert!(sql.contains(
            "\"__o\".\"odd\"\"name\" IS DISTINCT FROM \"__n\".\"odd\"\"name\" AS \"__changed0\""
        ));
        assert!(sql.contains("\"__o\".\"key\" IS DISTINCT FROM \"__n\".\"key\" AS \"__changed1\""));
        let first = format!(
            "{}, \"__u\".\"__changed0\", \"__u\".\"__changed1\" FROM",
            crate::NOW_UTC
        );
        let second = format!("{}, FALSE, FALSE FROM", crate::NOW_UTC);
        assert!(sql.contains(&first));
        assert!(sql.contains(&second));
        assert!(sql.contains("\"__u\".\"__r0\", \"__u\".\"__ro0\""));
    }

    #[test]
    fn changed_projection_works_for_get_or_create() {
        let columns = [("key", ColumnType::Int8)];
        let key = [KeyColumn {
            name: "key",
            nullable: true,
        }];
        let sql = upsert(
            &Upsert::new("t", "t_key", &columns, &key, ConflictAction::Nothing)
                .returning_changed(&["key"]),
        );
        assert!(sql.contains("DO NOTHING RETURNING WITH"));
        assert!(sql.contains("FALSE FROM \"__in\""));
    }

    #[test]
    fn key_definitions() {
        assert_eq!(
            key_definition(&["product_id", "lot_id"], false),
            "UNIQUE NULLS NOT DISTINCT (\"product_id\", \"lot_id\")"
        );
        assert_eq!(key_definition(&["code"], true), "UNIQUE (\"code\")");
    }

    #[test]
    fn upsert_statement_text() {
        let key = [
            KeyColumn {
                name: "product_id",
                nullable: false,
            },
            KeyColumn {
                name: "lot_id",
                nullable: true,
            },
        ];
        let columns = [
            ("product_id", ColumnType::Int8),
            ("lot_id", ColumnType::Int8),
            ("quantity", ColumnType::Float8),
            ("reserved", ColumnType::Float8),
            ("note", ColumnType::Varchar(None)),
        ];
        let sets = [
            ("quantity", SetAction::Accumulate),
            (
                "reserved",
                SetAction::AccumulateFloor {
                    param: 7,
                    ty: ColumnType::Float8,
                },
            ),
            ("note", SetAction::Overwrite),
        ];
        let sql = upsert(
            &Upsert::new(
                "t",
                "t_key",
                &columns,
                &key,
                ConflictAction::Update {
                    sets: &sets,
                    audit: &["write_uid"],
                },
            )
            .insert_exprs(&[("write_uid", "$6::int8"), ("row_version", "1")])
            .row_version(true)
            .returning(&[("quantity", ColumnType::Float8)])
            .returning_old(&[("note", ColumnType::Varchar(None))]),
        );
        assert_eq!(
            sql,
            "WITH \"__in\" AS MATERIALIZED (SELECT * FROM unnest($1::int8[], $2::int8[], \
             $3::float8[], $4::float8[], $5::varchar[]) WITH ORDINALITY AS \"__i\"(\"product_id\", \
             \"lot_id\", \"quantity\", \"reserved\", \"note\", \"__ord\")), \"__up\" AS (INSERT INTO \
             \"t\" AS \"__t\" (\"product_id\", \"lot_id\", \"quantity\", \"reserved\", \"note\", \
             \"write_uid\", \"row_version\") SELECT \"product_id\", \"lot_id\", \"quantity\", \
             \"reserved\", \"note\", $6::int8, 1 FROM \"__in\" ORDER BY \"__ord\" ON CONFLICT ON \
             CONSTRAINT \"t_key\" DO UPDATE SET \"quantity\" = COALESCE(\"__t\".\"quantity\", 0) + \
             EXCLUDED.\"quantity\", \"reserved\" = GREATEST($7::float8, \
             COALESCE(\"__t\".\"reserved\", 0) + EXCLUDED.\"reserved\"), \"note\" = \
             EXCLUDED.\"note\", \"row_version\" = \"__t\".\"row_version\" + 1, \"write_uid\" = \
             EXCLUDED.\"write_uid\" WHERE EXCLUDED.\"quantity\" <> 0 OR GREATEST($7::float8, \
             COALESCE(\"__t\".\"reserved\", 0) + EXCLUDED.\"reserved\") IS DISTINCT FROM \
             \"__t\".\"reserved\" OR \"__t\".\"note\" IS DISTINCT FROM EXCLUDED.\"note\" RETURNING \
             WITH (OLD AS \"__o\", NEW AS \"__n\") \"__n\".\"id\" AS \"__id\", \
             \"__n\".\"product_id\" AS \"__k0\", \"__n\".\"lot_id\" AS \"__k1\", \"__o\".\"id\" IS \
             NULL AS \"__inserted\", \"__o\".\"row_version\" AS \"__old_version\", \
             \"__n\".\"row_version\" AS \"__version\", \"__n\".\"quantity\" AS \"__r0\", \
             \"__o\".\"note\" AS \"__ro0\") SELECT \"__i\".\"__ord\", \"__u\".\"__id\", \
             \"__u\".\"__inserted\", TRUE, \"__u\".\"__old_version\", \"__u\".\"__version\", \
             \"__u\".\"__r0\", \"__u\".\"__ro0\", (now() AT TIME ZONE 'UTC') FROM \"__up\" AS \
             \"__u\" JOIN \"__in\" AS \"__i\" ON \"__u\".\"__k0\" = \"__i\".\"product_id\" AND \
             (\"__u\".\"__k1\" = \"__i\".\"lot_id\" OR (\"__u\".\"__k1\" IS NULL AND \
             \"__i\".\"lot_id\" IS NULL)) UNION ALL SELECT \"__i\".\"__ord\", \"__t\".\"id\", FALSE, \
             FALSE, \"__t\".\"row_version\", \"__t\".\"row_version\", \"__t\".\"quantity\", \
             NULL::varchar, \
             (now() AT TIME ZONE 'UTC') FROM \"__in\" AS \"__i\" JOIN \"t\" AS \"__t\" ON \
             \"__t\".\"product_id\" = \"__i\".\"product_id\" AND (\"__t\".\"lot_id\" = \
             \"__i\".\"lot_id\" OR (\"__t\".\"lot_id\" IS NULL AND \"__i\".\"lot_id\" IS NULL)) \
             WHERE NOT EXISTS (SELECT FROM \"__up\" AS \"__u\" WHERE \"__u\".\"__k0\" = \
             \"__i\".\"product_id\" AND (\"__u\".\"__k1\" = \"__i\".\"lot_id\" OR (\"__u\".\"__k1\" \
             IS NULL AND \"__i\".\"lot_id\" IS NULL)))"
        );
    }

    #[test]
    fn get_or_create_does_nothing_on_conflict() {
        let key = [KeyColumn {
            name: "code",
            nullable: false,
        }];
        let columns = [("code", ColumnType::Varchar(None))];
        let sql = upsert(&Upsert::new(
            "t",
            "t_code",
            &columns,
            &key,
            ConflictAction::Nothing,
        ));
        assert!(
            sql.contains("ON CONFLICT ON CONSTRAINT \"t_code\" DO NOTHING RETURNING"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "SELECT \"__i\".\"__ord\", \"__t\".\"id\", FALSE, FALSE, NULL::int8, NULL::int8,"
            ),
            "{sql}"
        );
    }

    #[test]
    fn write_precheck_text() {
        let key = [
            KeyColumn {
                name: "a",
                nullable: false,
            },
            KeyColumn {
                name: "b",
                nullable: true,
            },
        ];
        let joins = " LEFT JOIN \"o\" AS \"t__a\" ON (\"t\".\"a\" = \"t__a\".\"id\")";
        assert_eq!(
            existing_denied(
                "t",
                &key,
                &[ColumnType::Int8, ColumnType::Date],
                joins,
                PreCheckRules::new(Some("\"t__a\".\"x\" = $1::int8"), None),
                1
            ),
            "SELECT \"t\".\"id\", TRUE FROM \"t\" JOIN unnest($2::int8[], $3::date[]) AS \
             \"__k\"(\"a\", \"b\") ON \"t\".\"a\" = \"__k\".\"a\" AND (\"t\".\"b\" = \"__k\".\"b\" OR \
             (\"t\".\"b\" IS NULL AND \"__k\".\"b\" IS NULL)) LEFT JOIN \"o\" AS \"t__a\" ON \
             (\"t\".\"a\" = \"t__a\".\"id\") WHERE (\"t__a\".\"x\" = $1::int8) IS NOT TRUE"
        );
        // with a read rule: its parameters follow the write rule's, the key arrays come last
        assert_eq!(
            existing_denied(
                "t",
                &key[..1],
                &[ColumnType::Int8],
                joins,
                PreCheckRules::new(
                    Some("\"t__a\".\"x\" = $1::int8"),
                    Some("\"t\".\"c\" = $2::int8")
                ),
                2
            ),
            "SELECT \"t\".\"id\", (\"t\".\"c\" = $2::int8) IS TRUE FROM \"t\" JOIN \
             unnest($3::int8[]) AS \"__k\"(\"a\") ON \"t\".\"a\" = \"__k\".\"a\" LEFT JOIN \"o\" AS \
             \"t__a\" ON (\"t\".\"a\" = \"t__a\".\"id\") WHERE (\"t__a\".\"x\" = $1::int8) IS NOT TRUE OR \
             (\"t\".\"c\" = $2::int8) IS NOT TRUE"
        );
        // review SEC-M1: with no restricting write predicate the read predicate alone selects
        assert_eq!(
            existing_denied(
                "t",
                &key[..1],
                &[ColumnType::Int8],
                "",
                PreCheckRules::new(None, Some("\"t\".\"c\" = $1::int8")),
                1
            ),
            "SELECT \"t\".\"id\", (\"t\".\"c\" = $1::int8) IS TRUE FROM \"t\" JOIN \
             unnest($2::int8[]) AS \"__k\"(\"a\") ON \"t\".\"a\" = \"__k\".\"a\" WHERE \
             (\"t\".\"c\" = $1::int8) IS NOT TRUE"
        );
        // a model whose reads are refused: the read predicate is the constant FALSE
        assert!(
            existing_denied(
                "t",
                &key[..1],
                &[ColumnType::Int8],
                "",
                PreCheckRules::new(None, Some("FALSE")),
                0
            )
            .ends_with("WHERE (FALSE) IS NOT TRUE")
        );
    }
}
