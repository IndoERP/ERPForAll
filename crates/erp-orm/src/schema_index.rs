//! Index kinds in schema sync (design):
//!
//! - the extensions the declared indexes need are checked **before any DDL runs**: a trigram
//!   field index without `pg_trgm`, a typed `trigram` index without `pg_trgm` or a
//!   typed `gist` index without `btree_gist` fail the install naming the extension and the
//!   object, so nothing is created;
//! - typed model-level indexes are created from their definition and compared by the canonical
//!   text stored in their comment, like the raw table objects; a changed declaration, or an index
//!   of that name without the comment, is dropped and created again (the declaration owns the
//!   name), so a second sync issues no DDL;
//! - a field index whose access method, partial predicate or operator class no longer matches its
//!   kind is dropped and created again under the same name ('s rule, which also makes
//!   a change to `brin` or `gin` take effect); one that matches is left alone.

use erp_fields::{Error, ErrorKind, IndexKind, Result};
use erp_registry::{IndexDecl, ModelSpec, TableObjectKind};

use crate::exclusion::ExtensionNeed;
use erp_sql::ddl;
use erp_sql::filter::Unaccent;
use rustc_hash::FxHashSet;

/// What the catalog says about an existing index (`index_shape` rows of
/// [`ddl::CATALOG_QUERY`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexShape {
    /// `pg_am.amname`.
    pub(crate) method: String,
    /// The index has a predicate (`WHERE …`).
    pub(crate) partial: bool,
    /// The operator class of the first key column when it is not the column type's default.
    pub(crate) opclass: Option<String>,
}

impl IndexShape {
    /// Read one `index_shape` catalog row's `(a, b, c)`.
    pub(crate) fn from_row(
        index: &str,
        method: Option<String>,
        partial: Option<String>,
        opclass: Option<String>,
    ) -> Result<Self> {
        let method = method.ok_or_else(|| {
            Error::new(
                ErrorKind::Database,
                format!("the catalog gives no access method for index \"{index}\""),
            )
        })?;
        let partial = match partial.as_deref() {
            Some("true") => true,
            Some("false") => false,
            other => {
                return Err(Error::new(
                    ErrorKind::Database,
                    format!(
                        "unreadable partial flag {other:?} of index \"{index}\" in the catalog"
                    ),
                ));
            }
        };
        Ok(Self {
            method,
            partial,
            opclass,
        })
    }

    /// True when this index is what a field index of `kind` creates.
    pub(crate) fn matches(&self, kind: IndexKind) -> bool {
        self.method == kind.method()
            && self.partial == kind.not_null_only()
            && self.opclass.as_deref() == kind.opclass()
    }
}

/// `field '<model>.<field>' declares index = "<kind>", which needs the PostgreSQL extension
/// <ext>; it is not installed`.
pub(crate) fn missing_field_extension(
    model: &str,
    field: &str,
    kind: IndexKind,
    extension: &str,
) -> Error {
    Error::new(
        ErrorKind::Database,
        format!(
            "field '{model}.{field}' declares index = \"{}\", which needs the PostgreSQL \
             extension {extension}; it is not installed",
            kind.as_str()
        ),
    )
}

/// Refuse, before any DDL, the declared indexes whose extension is not installed (pg-features §6
/// finding 1; made the trigram case an error, this states it with the model and field).
/// The parent store's path index is always a btree and needs nothing.
///
/// This is the single extension gate of schema sync. `planned` are the extensions the sync itself
/// installs in its transaction ([`crate::Transaction::extension_needs`]: `btree_gist` for
/// exclusions and `gist` indexes, whatever a model lists in `extensions = [...]`); they count as
/// installed. Anything else the declared indexes need must already be there.
pub(crate) fn check_extensions(
    specs: &[&ModelSpec],
    installed: &FxHashSet<String>,
    planned: &[ExtensionNeed],
) -> Result<()> {
    let present = |extension: &str| {
        installed.contains(extension) || planned.iter().any(|(e, _, _)| e.name() == extension)
    };
    for spec in specs {
        for field in spec.column_fields() {
            let Some(kind) = field.index else { continue };
            if spec.parent_path == Some(field.id) {
                continue;
            }
            if let Some(extension) = kind.extension()
                && !present(extension)
            {
                return Err(missing_field_extension(
                    spec.name, field.name, kind, extension,
                ));
            }
        }
        for object in &spec.table_objects {
            if let TableObjectKind::TypedIndex(index) = object.kind
                && let Some(extension) = index.method.extension()
                && !present(extension)
            {
                return Err(Error::new(
                    ErrorKind::Database,
                    format!(
                        "index '{}' of '{}' declares method = \"{}\", which needs the PostgreSQL \
                         extension {extension}; it is not installed",
                        object.name,
                        spec.name,
                        index.method.as_str()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// What brings the typed index `full_name` to its declaration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TypedIndexPlan {
    /// The index is as declared: nothing to do.
    Keep,
    /// The index of that name is the declared one but carries no canonical comment (a build that
    /// stopped before it): only `COMMENT ON INDEX …` is missing.
    CommentOnly {
        /// `COMMENT ON INDEX …` carrying the canonical definition.
        comment: String,
    },
    /// Create it (`replaced`: an index of that name with another definition exists and is
    /// replaced; the declaration owns the name).
    Build {
        /// `CREATE INDEX …`.
        create: String,
        /// `COMMENT ON INDEX …` carrying the canonical definition.
        comment: String,
        /// Replace the existing index.
        replaced: bool,
    },
}

/// What the catalog says about the structure of a valid index (`index_struct` rows of
/// [`ddl::CATALOG_QUERY`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexStruct {
    pub(crate) method: String,
    /// Key columns in order; `(expr)` for an expression.
    pub(crate) keys: Vec<String>,
    pub(crate) include: Vec<String>,
    pub(crate) unique: bool,
    pub(crate) partial: bool,
    /// Operator class per key column; empty for the type's default.
    pub(crate) opclasses: Vec<String>,
    pub(crate) pred_expr: Option<String>,
    pub(crate) key_exprs: Option<Vec<String>>,
}

impl IndexStruct {
    /// Read one `index_struct` row's `(a, b, c)`.
    pub(crate) fn from_row(
        index: &str,
        method: Option<String>,
        columns: Option<String>,
        flags: Option<String>,
    ) -> Result<Self> {
        let bad = |what: &str| {
            Error::new(
                ErrorKind::Database,
                format!("unreadable {what} of index \"{index}\" in the catalog"),
            )
        };
        let method = method.ok_or_else(|| bad("access method"))?;
        let columns = columns.ok_or_else(|| bad("columns"))?;
        let (keys, include) = columns.split_once('|').ok_or_else(|| bad("columns"))?;
        let flags = flags.ok_or_else(|| bad("flags"))?;
        let parts: Vec<&str> = flags.split('\x1f').collect();
        if parts.len() != 3 {
            return Err(bad("flags"));
        }
        let flags_main = parts[0];
        let pred_expr = (!parts[1].is_empty()).then(|| parts[1].to_owned());
        let key_exprs = if !parts[2].is_empty() {
            Some(parts[2].split('\x1e').map(str::to_owned).collect())
        } else {
            None
        };
        let mut subparts = flags_main.splitn(3, ':');
        let flag = |text: Option<&str>| match text {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(bad("flags")),
        };
        let unique = flag(subparts.next())?;
        let partial = flag(subparts.next())?;
        if partial != pred_expr.is_some() {
            return Err(bad("flags"));
        }
        let list = |text: &str| -> Vec<String> {
            if text.is_empty() {
                Vec::new()
            } else {
                text.split(',').map(str::to_owned).collect()
            }
        };
        let opclasses = subparts.next().ok_or_else(|| bad("flags"))?;
        // one entry per key column, empty for defaults: keep the empty entries
        let opclasses = if keys.is_empty() {
            Vec::new()
        } else {
            opclasses.split(',').map(str::to_owned).collect()
        };
        Ok(Self {
            method,
            keys: list(keys),
            include: list(include),
            unique,
            partial,
            opclasses,
            pred_expr,
            key_exprs,
        })
    }

    /// True when this index has the structure of the typed index `index` with `predicate`: not
    /// unique, the same access method, the same key and `INCLUDE` columns in order, partial exactly
    /// when the declaration has a filter, the method's operator class on every key column, and
    /// matching filter expression and trigram folding expression (SEC-D2-1).
    fn is_declared(
        &self,
        index: &IndexDecl,
        canonical_predicate: Option<&str>,
        unaccent: Unaccent,
        is_translated: impl Fn(&str) -> bool,
    ) -> bool {
        let folded =
            index.method == erp_registry::IndexMethod::Trigram && unaccent == Unaccent::Indexable;
        let keys: Vec<&str> = index
            .fields
            .iter()
            .map(|f| {
                if (index.method == erp_registry::IndexMethod::Trigram && is_translated(f))
                    || folded
                {
                    "(expr)"
                } else {
                    *f
                }
            })
            .collect();
        let opclass = index.method.opclass().unwrap_or_default();
        if self.unique
            || self.method != index.method.access_method()
            || !self
                .keys
                .iter()
                .map(String::as_str)
                .eq(keys.iter().copied())
            || !self
                .include
                .iter()
                .map(String::as_str)
                .eq(index.include.iter().copied())
            || self.partial != canonical_predicate.is_some()
            || self.opclasses.len() != index.fields.len()
            || !self.opclasses.iter().all(|c| c == opclass)
        {
            return false;
        }
        if let Some(expected_pred) = canonical_predicate {
            let Some(catalog_pred) = &self.pred_expr else {
                return false;
            };
            if catalog_pred != expected_pred {
                return false;
            }
        }
        let has_expr = folded
            || (index.method == erp_registry::IndexMethod::Trigram
                && index.fields.iter().any(|f| is_translated(f)));
        if has_expr {
            let Some(catalog_exprs) = &self.key_exprs else {
                return false;
            };
            if catalog_exprs.len() != index.fields.len() {
                return false;
            }
            for (field, catalog_expr) in index.fields.iter().zip(catalog_exprs) {
                let trans = is_translated(field);
                if !matches_trigram_key(catalog_expr, field, trans) {
                    return false;
                }
            }
        }
        true
    }
}

/// Check whether a catalog key expression matches `erp_unaccent({field})` (M4):
/// strips outer parens, double quotes, and '::text' cast, but refuses wrappers like `lower(..)`
/// or extra expressions.
///
/// Note (L2): This assumes a text-cast column (`(field)::text`), which PostgreSQL emits when
/// indexing `char` or `text` fields. Any `COLLATE` clause or non-text cast is intentionally
/// refused, as foreign index adoption requires the exact expected text representation.
pub(crate) fn matches_trigram_key(expr: &str, field: &str, translated: bool) -> bool {
    let trimmed = expr.trim();
    let prefix = format!("{}(", erp_sql::filter::UNACCENT_WRAPPER);
    let inner = if trimmed.starts_with(&prefix) && trimmed.ends_with(')') {
        trimmed[prefix.len()..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    let mut s = inner;
    loop {
        if s.starts_with('(') && s.ends_with(')') {
            s = s[1..s.len() - 1].trim();
        } else if s.ends_with("::text") {
            s = s[..s.len() - 6].trim();
        } else {
            break;
        }
    }
    let has_jsonb = s.starts_with("jsonb_path_query_array(") && s.ends_with(')');
    if translated != has_jsonb {
        return false;
    }
    if has_jsonb {
        let inside = s["jsonb_path_query_array(".len()..s.len() - 1].trim();
        if let Some((first_arg, _)) = inside.split_once(',') {
            s = first_arg.trim();
        }
    }
    s = s.trim_matches('"');
    s == field
}

/// What [`typed_index_plan`] needs to know about one typed index and its database state.
pub(crate) struct TypedIndexInput<'a> {
    /// The model, for messages.
    pub(crate) model: &'a str,
    /// The declared name (`_by_date`), for messages.
    pub(crate) declared: &'a str,
    /// Declared in a model file or overlay (runtime data), not in Rust code.
    pub(crate) runtime: bool,
    pub(crate) table: &'a str,
    pub(crate) full_name: &'a str,
    pub(crate) index: &'a IndexDecl,
    /// Its compiled constant filter, if it has one.
    pub(crate) predicate: Option<&'a str>,
    /// The PostgreSQL-canonicalised form of `predicate` for structural adoption (M3).
    pub(crate) canonical_predicate: Option<&'a str>,
    pub(crate) unaccent: Unaccent,
    /// The catalog's comment of a valid index of that name on the table (`None`: no such index;
    /// `Some(None)`: one without a comment).
    pub(crate) existing: Option<&'a Option<String>>,
    /// The structure of that index.
    pub(crate) existing_struct: Option<&'a IndexStruct>,
    /// Whether a field is translated.
    pub(crate) is_translated: &'a (dyn Fn(&str) -> bool + 'a),
}

/// The plan for a typed index (see [`TypedIndexPlan`]).
///
/// The ORM's comment on an index starts with `INDEX USING `; only an index with that comment is
/// ours to replace. An index of the declared name without it (a foreign or DBA index, or a build
/// that stopped before its comment) is compared by structure ([`IndexStruct::is_declared`]): when it
/// is the declared one only the comment is written; otherwise a runtime declaration (a model
/// file, an overlay) is refused rather than dropping an index it did not create, and a native
/// declaration replaces it (the declaration owns the name) with a warning in the log.
///
/// # Errors
///
/// `Programming` for the runtime case above, naming the model, the index and the existing one.
pub(crate) fn typed_index_plan(input: &TypedIndexInput<'_>) -> Result<TypedIndexPlan> {
    let definition = ddl::typed_index_definition_with_translated(
        input.index,
        input.unaccent,
        input.predicate,
        input.is_translated,
    );
    let (create, comment) =
        ddl::create_table_object_index(input.table, input.full_name, &definition, false);
    let build = |replaced| TypedIndexPlan::Build {
        create: create.clone(),
        comment: comment.clone(),
        replaced,
    };
    match input.existing {
        None => return Ok(build(false)),
        Some(Some(found)) if *found == format!("INDEX {definition}") => {
            return Ok(TypedIndexPlan::Keep);
        }
        // the ORM's own comment, another definition: ours to replace
        Some(Some(found)) if found.starts_with("INDEX USING ") => return Ok(build(true)),
        // no comment, or one that is not the ORM's: judged by structure below
        Some(_) => {}
    }
    if input.existing_struct.is_some_and(|found| {
        found.is_declared(
            input.index,
            input.canonical_predicate,
            input.unaccent,
            input.is_translated,
        )
    }) {
        return Ok(TypedIndexPlan::CommentOnly { comment });
    }
    if input.runtime {
        return Err(Error::programming(format!(
            "index '{}' of '{}': the table already has an index named {} that the ORM did not \
             create (its comment is not the ORM's) and that differs from the declaration; a model \
             file does not replace it. If it is the leftover of an interrupted build, dropping it \
             (DROP INDEX {}) is safe and the next sync builds it; otherwise rename the declared \
             index",
            input.declared, input.model, input.full_name, input.full_name
        )));
    }
    tracing::warn!(
        target: "erp::orm",
        index = input.full_name,
        model = input.model,
        "replacing an index whose comment is not the ORM's by the declared one"
    );
    Ok(build(true))
}

#[cfg(test)]
mod tests {
    use erp_registry::IndexMethod;

    use super::*;

    fn shape(method: &str, partial: &str, opclass: Option<&str>) -> Result<IndexShape> {
        IndexShape::from_row(
            "i",
            Some(method.to_owned()),
            Some(partial.to_owned()),
            opclass.map(str::to_owned),
        )
    }

    #[test]
    fn shapes_match_their_kinds_only() {
        let btree = shape("btree", "false", None).unwrap();
        let partial = shape("btree", "true", None).unwrap();
        let brin = shape("brin", "false", None).unwrap();
        let trigram = shape("gin", "false", Some("gin_trgm_ops")).unwrap();
        let jsonb = shape("gin", "false", Some("jsonb_path_ops")).unwrap();
        let cases = [
            (&btree, IndexKind::Btree),
            (&partial, IndexKind::BtreeNotNull),
            (&brin, IndexKind::Brin),
            (&trigram, IndexKind::Trigram),
            (&jsonb, IndexKind::Gin),
        ];
        for (i, (shape, kind)) in cases.iter().enumerate() {
            for (j, (_, other)) in cases.iter().enumerate() {
                assert_eq!(
                    shape.matches(*other),
                    i == j,
                    "{shape:?} vs {other:?} ({kind:?})"
                );
            }
        }
    }

    #[test]
    fn unreadable_catalog_rows_are_errors() {
        let err = IndexShape::from_row("i", None, Some("false".into()), None).unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Database,
                "the catalog gives no access method for index \"i\""
            )
        );
        let err = shape("btree", "t", None).unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Database,
                "unreadable partial flag Some(\"t\") of index \"i\" in the catalog"
            )
        );
        let err = IndexShape::from_row("i", Some("btree".into()), None, None).unwrap_err();
        assert_eq!(
            err.message(),
            "unreadable partial flag None of index \"i\" in the catalog"
        );
    }

    fn input<'a>(
        index: &'a IndexDecl,
        predicate: Option<&'a str>,
        canonical_predicate: Option<&'a str>,
        unaccent: Unaccent,
        existing: Option<&'a Option<String>>,
        existing_struct: Option<&'a IndexStruct>,
        runtime: bool,
    ) -> TypedIndexInput<'a> {
        TypedIndexInput {
            model: "m.x",
            declared: "_i",
            runtime,
            table: "t",
            full_name: "t_i",
            index,
            predicate,
            canonical_predicate,
            unaccent,
            existing,
            existing_struct,
            is_translated: &|_| false,
        }
    }

    fn input_with_translated<'a>(
        index: &'a IndexDecl,
        unaccent: Unaccent,
        existing: Option<&'a Option<String>>,
        existing_struct: Option<&'a IndexStruct>,
        runtime: bool,
        is_translated: &'a (dyn Fn(&str) -> bool + 'a),
    ) -> TypedIndexInput<'a> {
        TypedIndexInput {
            model: "m.x",
            declared: "_i",
            runtime,
            table: "t",
            full_name: "t_i",
            index,
            predicate: None,
            canonical_predicate: None,
            unaccent,
            existing,
            existing_struct,
            is_translated,
        }
    }

    fn found(method: &str, columns: &str, flags: &str) -> IndexStruct {
        IndexStruct::from_row(
            "t_i",
            Some(method.to_owned()),
            Some(columns.to_owned()),
            Some(flags.to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn typed_index_plans_follow_the_comment() {
        let index = IndexDecl::new(&["a", "b"])
            .method(IndexMethod::Btree)
            .include(&["c"]);
        let definition = ddl::typed_index_definition(&index, Unaccent::Off, None);
        let (create, comment) = ddl::create_table_object_index("t", "t_i", &definition, false);
        let build = |replaced| TypedIndexPlan::Build {
            create: create.clone(),
            comment: comment.clone(),
            replaced,
        };
        let plan = |existing, shape, runtime| {
            typed_index_plan(&input(
                &index,
                None,
                None,
                Unaccent::Off,
                existing,
                shape,
                runtime,
            ))
        };
        // missing: created with its comment
        assert_eq!(plan(None, None, false).unwrap(), build(false));
        // the same canonical definition: nothing
        let same = Some(format!("INDEX {definition}"));
        assert_eq!(
            plan(Some(&same), None, false).unwrap(),
            TypedIndexPlan::Keep
        );
        // another definition in the ORM's own comment: replaced, runtime declarations too
        let other = Some("INDEX USING btree (\"a\")".to_owned());
        assert_eq!(plan(Some(&other), None, true).unwrap(), build(true));
        // no comment, and the catalog shows the declared structure: only the comment is written
        let ours = found("btree", "a,b|c", "false:false:,\x1f\x1f");
        assert_eq!(
            plan(Some(&None), Some(&ours), true).unwrap(),
            TypedIndexPlan::CommentOnly {
                comment: comment.clone()
            }
        );
        // a comment that is not the ORM's is judged the same way
        let foreign_comment = Some("made by hand".to_owned());
        assert_eq!(
            plan(Some(&foreign_comment), Some(&ours), true).unwrap(),
            TypedIndexPlan::CommentOnly {
                comment: comment.clone()
            }
        );
        // a different structure: a native declaration replaces it ...
        let hash = found("hash", "a,b|c", "false:false:,\x1f\x1f");
        let unique = found("btree", "a,b|c", "true:false:,\x1f\x1f");
        let fewer = found("btree", "a|", "false:false:\x1f\x1f");
        for shape in [&hash, &unique, &fewer] {
            assert_eq!(plan(Some(&None), Some(shape), false).unwrap(), build(true));
            assert_eq!(
                plan(Some(&foreign_comment), Some(shape), false).unwrap(),
                build(true)
            );
            // ... a runtime declaration is refused
            let err = plan(Some(&None), Some(shape), true).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(
                err.message(),
                "index '_i' of 'm.x': the table already has an index named t_i that the ORM did \
                 not create (its comment is not the ORM's) and that differs from the declaration; \
                 a model file does not replace it. If it is the leftover of an interrupted build, \
                 dropping it (DROP INDEX t_i) is safe and the next sync builds it; otherwise \
                 rename the declared index"
            );
        }
        // a filtered index is partial; a trigram key is an expression when accents are folded
        let filtered = IndexDecl::new(&["a"]);
        let partial = found("btree", "a|", "false:true:\x1f((a)::text = 'x'::text)\x1f");
        let plain = found("btree", "a|", "false:false:\x1f\x1f");
        let with_filter = |shape, canon| {
            typed_index_plan(&input(
                &filtered,
                Some("\"a\" = 'x'"),
                canon,
                Unaccent::Off,
                Some(&None),
                Some(shape),
                true,
            ))
        };
        assert!(matches!(
            with_filter(&partial, Some("((a)::text = 'x'::text)")).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        assert!(with_filter(&plain, Some("((a)::text = 'x'::text)")).is_err());
        assert!(with_filter(&partial, None).is_err());
        // SEC-D2-1: compare canonical pg_get_expr(indpred)
        let matching_pred = found("btree", "a|", "false:true:\x1f((a)::text = 'x'::text)\x1f");
        assert!(matches!(
            with_filter(&matching_pred, Some("((a)::text = 'x'::text)")).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        let different_pred = found("btree", "a|", "false:true:\x1f((a)::text = 'y'::text)\x1f");
        assert!(with_filter(&different_pred, Some("((a)::text = 'x'::text)")).is_err());
        let trigram = IndexDecl::new(&["name"]).method(IndexMethod::Trigram);
        let trgm = |shape, unaccent| {
            typed_index_plan(&input(
                &trigram,
                None,
                None,
                unaccent,
                Some(&None),
                Some(shape),
                true,
            ))
        };
        let columns = found("gin", "name|", "false:false:gin_trgm_ops\x1f\x1f");
        let expression = found(
            "gin",
            "(expr)|",
            "false:false:gin_trgm_ops\x1f\x1ferp_unaccent((name)::text)",
        );
        assert!(matches!(
            trgm(&columns, Unaccent::Off).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        assert!(matches!(
            trgm(&expression, Unaccent::Indexable).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        assert!(trgm(&columns, Unaccent::Indexable).is_err());
        // SEC-D2-1, M4: trigram expression check with folded unaccent
        let matching_expr = found(
            "gin",
            "(expr)|",
            "false:false:gin_trgm_ops\x1f\x1ferp_unaccent((name)::text)",
        );
        assert!(matches!(
            trgm(&matching_expr, Unaccent::Indexable).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        let foreign_expr = found(
            "gin",
            "(expr)|",
            "false:false:gin_trgm_ops\x1f\x1flower(erp_unaccent((name)::text))",
        );
        assert!(trgm(&foreign_expr, Unaccent::Indexable).is_err());
        let trigram_trans = IndexDecl::new(&["name"]).method(IndexMethod::Trigram);
        let trgm_trans = |shape, unaccent| {
            typed_index_plan(&input_with_translated(
                &trigram_trans,
                unaccent,
                Some(&None),
                Some(shape),
                true,
                &|f| f == "name",
            ))
        };
        let foreign_trans_expr = found(
            "gin",
            "(expr)|",
            "false:false:gin_trgm_ops\x1f\x1f(jsonb_path_query_array(\"other\", '$.*')::text)",
        );
        let matching_trans_expr = found(
            "gin",
            "(expr)|",
            "false:false:gin_trgm_ops\x1f\x1f(jsonb_path_query_array(\"name\", '$.*')::text)",
        );
        assert!(matches!(
            trgm_trans(&matching_trans_expr, Unaccent::Off).unwrap(),
            TypedIndexPlan::CommentOnly { .. }
        ));
        assert!(trgm_trans(&foreign_trans_expr, Unaccent::Off).is_err());
        // a filter is part of the definition: adding one replaces the index
        assert!(matches!(
            typed_index_plan(&input(
                &index,
                Some("\"a\" = 'x'"),
                None,
                Unaccent::Off,
                Some(&same),
                None,
                false
            ))
            .unwrap(),
            TypedIndexPlan::Build { replaced: true, .. }
        ));
    }

    #[test]
    fn unreadable_structure_rows_are_errors() {
        let err = IndexStruct::from_row(
            "i",
            Some("btree".into()),
            Some("a".into()),
            Some("x\x1f\x1f".into()),
        )
        .unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Database,
                "unreadable columns of index \"i\" in the catalog"
            )
        );
        // malformed flags (M5): 0, 1, 3 separators, invalid booleans, partial mismatch
        for invalid_flags in [
            "false:false:",
            "false:false:\x1f",
            "false:false:\x1f\x1f\x1f",
            "maybe:false:\x1f\x1f",
            // partial with missing pred_expr
            "false:true:\x1f\x1f",
            // non-partial with present pred_expr
            "false:false:\x1f((a)::text = 'x'::text)\x1f",
        ] {
            let err = IndexStruct::from_row(
                "i",
                Some("btree".into()),
                Some("a|".into()),
                Some(invalid_flags.into()),
            )
            .unwrap_err();
            assert_eq!(
                err.message(),
                "unreadable flags of index \"i\" in the catalog",
                "failed for flags {invalid_flags:?}"
            );
        }
    }

    #[test]
    fn missing_extension_messages() {
        let err = missing_field_extension("scale.item", "name", IndexKind::Trigram, "pg_trgm");
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::Database,
                "field 'scale.item.name' declares index = \"trigram\", which needs the PostgreSQL \
                 extension pg_trgm; it is not installed"
            )
        );
    }
}
