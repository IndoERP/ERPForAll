//! Row versions.
//!
//! - [`bump_function`] and [`bump_trigger`]: the `BEFORE UPDATE` trigger that makes every
//!   `UPDATE` of a versioned row — ORM, raw SQL, migration — bump `row_version` by exactly one. The
//!   ORM's own statements set `row_version = row_version + 1` themselves, so the trigger's `WHEN`
//!   clause is false for them and the function body runs only for other SQL.
//! - [`update_from_arrays`]: the flush `UPDATE … FROM unnest(…)` with the version bump, the
//!   expected-version check of `expect_versions` and `RETURNING` old values for write hooks.
//!
//! Schema sync stores a canonical text as the comment of the function and of each trigger and
//! re-creates them when the comment differs or they are missing (the table-object rule).

use std::sync::Arc;

use erp_fields::ColumnType;
use erp_registry::{ModelSpec, Registry};

use crate::{literal, quote, shorten_identifier};

/// The shared trigger function's name.
pub const BUMP_FUNCTION: &str = "erp_row_version_bump";

/// Comment stored on the function: the version of its body (change it with the body).
pub const BUMP_FUNCTION_COMMENT: &str = "erp row version bump v1";

/// The function and its comment (two statements). The body names the operator's schema, so the
/// caller's `search_path` cannot change what `+` means.
pub fn bump_function() -> Vec<String> {
    vec![
        format!(
            "CREATE OR REPLACE FUNCTION {BUMP_FUNCTION}() RETURNS trigger LANGUAGE plpgsql AS \
             $erp$ BEGIN NEW.row_version := OLD.row_version OPERATOR(pg_catalog.+) 1; RETURN NEW; \
             END $erp$"
        ),
        format!(
            "COMMENT ON FUNCTION {BUMP_FUNCTION}() IS {}",
            literal(BUMP_FUNCTION_COMMENT)
        ),
    ]
}

/// `ALTER TABLE … ALTER COLUMN row_version SET DEFAULT 1`: the one ORM column that keeps a
/// database default, so a raw `INSERT` that leaves the version out (migrations, addon SQL) gets
/// version 1 instead of a NOT NULL violation. The ORM always sends the version itself.
pub fn set_default(table: &str) -> String {
    format!(
        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT 1",
        quote(table),
        quote("row_version")
    )
}

/// The trigger's name on `table`: `<table>__row_version`, shortened.
pub fn trigger_name(table: &str) -> String {
    shorten_identifier(&format!("{table}__row_version"))
}

/// The canonical definition of the trigger on any table (stored as its comment).
pub const TRIGGER_DEFINITION: &str = "BEFORE UPDATE FOR EACH ROW WHEN (NEW.row_version IS \
                                      DISTINCT FROM OLD.row_version + 1) EXECUTE FUNCTION \
                                      erp_row_version_bump()";

/// `CREATE TRIGGER <table>__row_version BEFORE UPDATE ON <table> FOR EACH ROW WHEN (…) EXECUTE
/// FUNCTION erp_row_version_bump()` and its comment, after dropping a trigger of that name that
/// does not match.
pub fn bump_trigger(table: &str, replace: bool) -> Vec<String> {
    let name = quote(&trigger_name(table));
    let t = quote(table);
    let mut out = Vec::with_capacity(3);
    if replace {
        out.push(format!("DROP TRIGGER IF EXISTS {name} ON {t}"));
    }
    out.push(format!(
        "CREATE TRIGGER {name} BEFORE UPDATE ON {t} FOR EACH ROW WHEN (NEW.{rv} IS DISTINCT FROM \
         OLD.{rv} + 1) EXECUTE FUNCTION {BUMP_FUNCTION}()",
        rv = quote("row_version")
    ));
    out.push(format!(
        "COMMENT ON TRIGGER {name} ON {t} IS {}",
        literal(TRIGGER_DEFINITION)
    ));
    out
}

/// What the flush `UPDATE` adds to the plain one ([`crate::dml::update_from_arrays`]).
///
/// ```
/// use erp_sql::version::VersionedUpdate;
/// let update = VersionedUpdate::new().returning_changed(&["name"]);
/// assert!(update.returns_rows());
/// ```
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct VersionedUpdate<'a> {
    /// The table has a row version: `SET row_version = row_version + 1`.
    pub bump: bool,
    /// Bind the expected versions as the last array and update only rows whose version equals
    /// it where it is not NULL (`expect_versions`).
    pub expected: bool,
    /// Columns returned as NULL-safe OLD/NEW difference flags, in this order.
    /// Returned rows contain id, the hook OLD/NEW pairs, then these boolean flags.
    /// A nonempty projection returns rows even without expected-version checks or hooks.
    pub returning_changed: &'a [&'a str],
    /// Columns whose old and new values are returned (write hooks); with `expected` or with
    /// these, the statement returns the id of every updated row first.
    pub returning_old: &'a [&'a str],
    /// Columns merged as JSONB deltas: `CASE WHEN "__v".c IS NULL THEN NULL ELSE jsonb_strip_nulls(COALESCE("t".c, '{}'::jsonb) || "__v".c) END`.
    pub merge_json: &'a [&'a str],
    /// Company patches: each key maps to `[value]` (set, including null) or `[]` (remove).
    /// Company patches take precedence over translation merging for a named column.
    pub merge_company: &'a [&'a str],
}

impl<'a> VersionedUpdate<'a> {
    /// No additions: the plain statement.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the row version.
    #[must_use]
    pub fn bump(mut self, on: bool) -> Self {
        self.bump = on;
        self
    }

    /// Check expected versions (bound as the last `int8[]` array).
    #[must_use]
    pub fn expected(mut self, on: bool) -> Self {
        self.expected = on;
        self
    }

    /// Return old and new values of these columns.
    #[must_use]
    pub fn returning_old(mut self, columns: &'a [&'a str]) -> Self {
        self.returning_old = columns;
        self
    }

    /// Return NULL-safe difference flags for these physical columns, after hook values.
    /// Comparisons use the final database NEW value, including JSON merges and triggers.
    #[must_use]
    pub fn returning_changed(mut self, columns: &'a [&'a str]) -> Self {
        self.returning_changed = columns;
        self
    }

    /// Merge deltas for these JSONB columns instead of whole-cell replacement.
    #[must_use]
    pub fn merge_json(mut self, columns: &'a [&'a str]) -> Self {
        self.merge_json = columns;
        self
    }

    /// Atomically apply company patches, preserving untouched keys and explicit nulls.
    /// Empty resulting maps are SQL NULL. The caller validates patch keys and values;
    /// SQL NULL is an empty patch, not a request to clear all companies.
    #[must_use]
    pub fn merge_company(mut self, columns: &'a [&'a str]) -> Self {
        self.merge_company = columns;
        self
    }

    /// Whether the statement returns rows (the updated ids).
    pub fn returns_rows(&self) -> bool {
        self.expected || !self.returning_old.is_empty() || !self.returning_changed.is_empty()
    }
}

/// [`crate::dml::update_from_arrays`] with the [`VersionedUpdate`] additions:
///
/// ```sql
/// UPDATE "t" SET "a" = "__v"."a", "write_date" = (now() AT TIME ZONE 'UTC'),
///        "row_version" = "t"."row_version" + 1
///   FROM (SELECT * FROM unnest($1::int8[], $2::varchar[], $3::int8[])
///         AS "__a"("id", "a", "__expected") ORDER BY "id") AS "__v"
///  WHERE "t"."id" = "__v"."id"
///    AND ("__v"."__expected" IS NULL OR "t"."row_version" = "__v"."__expected")
/// RETURNING WITH (OLD AS "__o", NEW AS "__n") "__n"."id", "__o"."a", "__n"."a"
/// ```
///
/// Rows come back in no particular order: callers map them by id.
pub fn update_from_arrays(
    table: &str,
    columns: &[(&str, ColumnType)],
    extra: &[(&str, &str)],
    with: &VersionedUpdate<'_>,
) -> String {
    update_from_arrays_quoted(&quote(table), columns, extra, with)
}

/// [`update_from_arrays`] with the table already quoted (`"table"`), so a caller holding a
/// pre-quoted name (the registry) does not quote it again. Same text as `update_from_arrays`.
fn update_from_arrays_quoted(
    t: &str,
    columns: &[(&str, ColumnType)],
    extra: &[(&str, &str)],
    with: &VersionedUpdate<'_>,
) -> String {
    use std::fmt::Write;
    let rv = quote("row_version");
    let cap =
        256 + t.len() * 4 + columns.len() * 60 + extra.len() * 40 + with.returning_old.len() * 30;
    let mut out = String::with_capacity(cap);
    out.push_str("UPDATE ");
    out.push_str(t);
    out.push_str(" SET ");
    let mut first = true;
    for (c, _) in columns {
        if !first {
            out.push_str(", ");
        } else {
            first = false;
        }
        let q = quote(c);
        out.push_str(&q);
        out.push_str(" = ");
        if with.merge_company.contains(c) {
            let base = format!("{t}.{q}");
            let patch = format!("\"__v\".{q}");
            out.push_str(&company_patch_expression(&base, &patch));
        } else if with.merge_json.contains(c) {
            out.push_str("CASE WHEN \"__v\".");
            out.push_str(&q);
            out.push_str(" IS NULL THEN NULL ELSE jsonb_strip_nulls(COALESCE(");
            out.push_str(t);
            out.push('.');
            out.push_str(&q);
            out.push_str(", '{}'::jsonb) || \"__v\".");
            out.push_str(&q);
            out.push_str(") END");
        } else {
            out.push_str("\"__v\".");
            out.push_str(&q);
        }
    }
    for (c, e) in extra {
        if !first {
            out.push_str(", ");
        } else {
            first = false;
        }
        out.push_str(&quote(c));
        out.push_str(" = ");
        out.push_str(e);
    }
    if with.bump {
        if !first {
            out.push_str(", ");
        }
        out.push_str(&rv);
        out.push_str(" = ");
        out.push_str(t);
        out.push('.');
        out.push_str(&rv);
        out.push_str(" + 1");
    }
    out.push_str(" FROM (SELECT * FROM unnest($1::int8[]");
    for (i, (_, ty)) in columns.iter().enumerate() {
        let _ = write!(out, ", ${}::{}[]", i + 2, ty.udt_name());
    }
    if with.expected {
        let _ = write!(out, ", ${}::int8[]", columns.len() + 2);
    }
    out.push_str(") AS \"__a\"(\"id\"");
    for (c, _) in columns {
        out.push_str(", ");
        out.push_str(&quote(c));
    }
    if with.expected {
        out.push_str(", \"__expected\"");
    }
    out.push_str(") ORDER BY \"id\") AS \"__v\" WHERE ");
    out.push_str(t);
    out.push_str(".\"id\" = \"__v\".\"id\"");
    if with.expected {
        out.push_str(" AND (\"__v\".\"__expected\" IS NULL OR ");
        out.push_str(t);
        out.push('.');
        out.push_str(&rv);
        out.push_str(" = \"__v\".\"__expected\")");
    }
    if with.returns_rows() {
        out.push_str(" RETURNING WITH (OLD AS \"__o\", NEW AS \"__n\") \"__n\".\"id\"");
        for c in with.returning_old {
            let qc = quote(c);
            out.push_str(", \"__o\".");
            out.push_str(&qc);
            out.push_str(", \"__n\".");
            out.push_str(&qc);
        }
        for c in with.returning_changed {
            let qc = quote(c);
            out.push_str(", (\"__o\".");
            out.push_str(&qc);
            out.push_str(" IS DISTINCT FROM \"__n\".");
            out.push_str(&qc);
            out.push(')');
        }
    }
    out
}

/// Apply a validated company patch to a JSONB map. SQL fragments come from the
/// quoted statement builder, never from user values. Each delta entry is either
/// an empty array (remove) or a singleton array (set, including JSON null).
fn company_patch_expression(base: &str, patch: &str) -> String {
    format!(
        "NULLIF((COALESCE({base}, '{{}}'::jsonb) - ARRAY(SELECT key FROM \"pg_catalog\".jsonb_each({patch}))) \
         || COALESCE((SELECT jsonb_object_agg(key, (SELECT element FROM \"pg_catalog\".jsonb_array_elements(value) AS delta(element))) FROM \"pg_catalog\".jsonb_each({patch}) \
         WHERE jsonb_array_length(value) > 0), '{{}}'::jsonb), '{{}}'::jsonb)"
    )
}

/// Cached statement text for `update_from_arrays` (PERF.SQL-TEXT). A cache hit is meant
/// to allocate nothing (by construction; not measured, DEFERRED_PERFORMANCE).
pub fn update_from_arrays_cached(
    registry: &Registry,
    spec: &ModelSpec,
    columns: &[(&str, ColumnType)],
    extra: &[(&str, &str)],
    with: &VersionedUpdate<'_>,
) -> Arc<str> {
    registry.statement_cache_of(spec).update_from_arrays(
        columns,
        extra,
        erp_registry::statement_cache::UpdateShape::new()
            .bump(with.bump)
            .expected(with.expected)
            .returning_old(with.returning_old)
            .returning_changed(with.returning_changed)
            .merge_json(with.merge_json)
            .merge_company(with.merge_company),
        || update_from_arrays_quoted(spec.quoted_table(), columns, extra, with),
    )
}

/// `SELECT "id", "row_version" FROM t WHERE "id" = ANY($1)`: the classification read after an
/// expected-version check failed (stale or deleted).
pub fn current_versions(table: &str) -> String {
    format!(
        "SELECT {id}, {rv} FROM {t} WHERE {id} = ANY($1)",
        id = quote("id"),
        rv = quote("row_version"),
        t = quote(table)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_projection_preserves_hook_layout_and_order() {
        let columns = [("name", ColumnType::Jsonb)];
        let with = VersionedUpdate::new()
            .bump(true)
            .expected(true)
            .returning_old(&["name"])
            .returning_changed(&["odd\"name", "name"])
            .merge_json(&["name"]);
        let sql = update_from_arrays("odd\"table", &columns, &[], &with);
        assert!(with.returns_rows());
        assert!(sql.ends_with(
            "RETURNING WITH (OLD AS \"__o\", NEW AS \"__n\") \"__n\".\"id\", \"__o\".\"name\", \"__n\".\"name\", (\"__o\".\"odd\"\"name\" IS DISTINCT FROM \"__n\".\"odd\"\"name\"), (\"__o\".\"name\" IS DISTINCT FROM \"__n\".\"name\")"
        ), "{sql}");
        assert!(
            sql.contains("COALESCE(\"odd\"\"table\".\"name\", '{}'::jsonb)"),
            "{sql}"
        );
        assert!(sql.contains("\"__v\".\"__expected\" IS NULL"), "{sql}");
    }

    #[test]
    fn changed_projection_alone_returns_ids_and_empty_projection_is_legacy() {
        let columns = [("a", ColumnType::Int8)];
        let empty = VersionedUpdate::new().returning_changed(&[]);
        assert!(!empty.returns_rows());
        assert_eq!(
            update_from_arrays("t", &columns, &[], &empty),
            crate::dml::update_from_arrays("t", &columns, &[])
        );
        let with = VersionedUpdate::new().returning_changed(&["a"]);
        assert!(with.returns_rows());
        assert!(update_from_arrays("t", &columns, &[], &with).ends_with(
            "RETURNING WITH (OLD AS \"__o\", NEW AS \"__n\") \"__n\".\"id\", (\"__o\".\"a\" IS DISTINCT FROM \"__n\".\"a\")"
        ));
    }

    #[test]
    fn cached_changed_projection_matches_uncached_and_keeps_order() {
        use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "test_changes",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("test_changes", "test.changes")
            .fields(&[FieldDecl::char("name"), FieldDecl::integer("amount")]);
        let registry = RegistryBuilder::new([&ADDON])
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let spec = registry.model("test.changes").unwrap();
        let columns = [("name", ColumnType::Varchar(None))];
        let mut cached = Vec::new();
        for names in [
            &[][..],
            &["name"][..],
            &["amount"][..],
            &["name", "amount"][..],
            &["amount", "name"][..],
        ] {
            let with = VersionedUpdate::new()
                .returning_old(&["name"])
                .returning_changed(names);
            let sql = update_from_arrays_cached(&registry, spec, &columns, &[], &with);
            assert_eq!(
                &*sql,
                update_from_arrays("test_changes", &columns, &[], &with)
            );
            let hit = update_from_arrays_cached(&registry, spec, &columns, &[], &with);
            assert!(Arc::ptr_eq(&sql, &hit));
            cached.push(sql);
        }
        for (index, sql) in cached.iter().enumerate() {
            for other in &cached[index + 1..] {
                assert_ne!(sql, other);
                assert!(!Arc::ptr_eq(sql, other));
            }
        }
    }

    #[test]
    fn trigger_and_function_texts() {
        assert_eq!(
            bump_trigger("scale_quant", false),
            vec![
                "CREATE TRIGGER \"scale_quant__row_version\" BEFORE UPDATE ON \"scale_quant\" FOR \
                 EACH ROW WHEN (NEW.\"row_version\" IS DISTINCT FROM OLD.\"row_version\" + 1) \
                 EXECUTE FUNCTION erp_row_version_bump()"
                    .to_owned(),
                format!(
                    "COMMENT ON TRIGGER \"scale_quant__row_version\" ON \"scale_quant\" IS '{}'",
                    TRIGGER_DEFINITION
                ),
            ]
        );
        assert_eq!(
            bump_trigger("t", true)[0],
            "DROP TRIGGER IF EXISTS \"t__row_version\" ON \"t\""
        );
        let f = bump_function();
        assert!(
            f[0].contains("NEW.row_version := OLD.row_version OPERATOR(pg_catalog.+) 1"),
            "{}",
            f[0]
        );
        assert_eq!(
            f[1],
            "COMMENT ON FUNCTION erp_row_version_bump() IS 'erp row version bump v1'"
        );
    }

    #[test]
    fn versioned_update_texts() {
        let cols = [("a", ColumnType::Varchar(None))];
        let extra = [("write_date", crate::NOW_UTC)];
        // the plain statement when nothing is added
        assert_eq!(
            update_from_arrays("t", &cols, &extra, &VersionedUpdate::new()),
            crate::dml::update_from_arrays("t", &cols, &extra)
        );
        assert_eq!(
            update_from_arrays(
                "t",
                &cols,
                &extra,
                &VersionedUpdate::new()
                    .bump(true)
                    .expected(true)
                    .returning_old(&["a"])
            ),
            "UPDATE \"t\" SET \"a\" = \"__v\".\"a\", \"write_date\" = (now() AT TIME ZONE 'UTC'), \
             \"row_version\" = \"t\".\"row_version\" + 1 FROM (SELECT * FROM unnest($1::int8[], \
             $2::varchar[], $3::int8[]) AS \"__a\"(\"id\", \"a\", \"__expected\") ORDER BY \"id\") \
             AS \"__v\" WHERE \"t\".\"id\" = \"__v\".\"id\" AND (\"__v\".\"__expected\" IS NULL OR \
             \"t\".\"row_version\" = \"__v\".\"__expected\") RETURNING WITH (OLD AS \"__o\", NEW \
             AS \"__n\") \"__n\".\"id\", \"__o\".\"a\", \"__n\".\"a\""
        );
        assert_eq!(
            update_from_arrays(
                "t",
                &[("name", ColumnType::Jsonb)],
                &extra,
                &VersionedUpdate::new().merge_json(&["name"])
            ),
            "UPDATE \"t\" SET \"name\" = CASE WHEN \"__v\".\"name\" IS NULL THEN NULL ELSE jsonb_strip_nulls(COALESCE(\"t\".\"name\", '{}'::jsonb) || \"__v\".\"name\") END, \"write_date\" = (now() AT TIME ZONE 'UTC') FROM (SELECT * FROM unnest($1::int8[], $2::jsonb[]) AS \"__a\"(\"id\", \"name\") ORDER BY \"id\") AS \"__v\" WHERE \"t\".\"id\" = \"__v\".\"id\""
        );
        assert_eq!(
            update_from_arrays("t", &cols, &[], &VersionedUpdate::new().bump(true)),
            "UPDATE \"t\" SET \"a\" = \"__v\".\"a\", \"row_version\" = \"t\".\"row_version\" + 1 \
             FROM (SELECT * FROM unnest($1::int8[], $2::varchar[]) AS \"__a\"(\"id\", \"a\") ORDER \
             BY \"id\") AS \"__v\" WHERE \"t\".\"id\" = \"__v\".\"id\""
        );
        assert_eq!(
            current_versions("t"),
            "SELECT \"id\", \"row_version\" FROM \"t\" WHERE \"id\" = ANY($1)"
        );
    }
}
