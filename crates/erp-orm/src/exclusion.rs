//! Typed exclusion constraints and `WITHOUT OVERLAPS` keys in the ORM (design §5, §10;
//! sub-slice).
//!
//! - [`Exclusions`] compiles every declared exclusion and temporal key when the ORM is built: the
//!   constant `filter` (a bad filter fails the build), the canonical constraint definition stored
//!   in the constraint's comment, and the period `CHECK`.
//! - Schema sync installs the extensions they need ([`Transaction::install_extensions`]) and
//!   creates, keeps or re-creates the constraints ([`sync_statements`]).
//! - `create` and `write` refuse empty or inverted periods before any statement
//!   ([`check_rows`], [`check_write`]); the generated `CHECK` refuses them in the database, and its
//!   violation gets the same field-named message ([`period_check_message`]).

use erp_fields::{Cell, Error, ErrorKind, RangeBounds, RecId, Result};
use erp_registry::{
    FieldId, FieldSpec, IndexMethod, ModelId, ModelSpec, PgExtension, Registry, TableObject,
    TableObjectKind, describe_object,
};
use erp_sql::conflict::{ConflictObject, KeyMatch, conflict_pairs_query};
use erp_sql::exclusion::{
    add_check, compiled_period, compiled_range, create_extension, exclusion_definition,
    period_check_definition, period_check_name, temporal_key_definition,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;

/// Which typed table object an install refusal, sync result or `Orm::build_object` report is
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectKind {
    /// An exclusion constraint.
    Exclusion,
    /// A `WITHOUT OVERLAPS` temporal key.
    TemporalKey,
}

impl ObjectKind {
    /// The phrase messages use for the kind.
    const fn phrase(self) -> &'static str {
        match self {
            Self::Exclusion => "an exclusion constraint",
            Self::TemporalKey => "a WITHOUT OVERLAPS key",
        }
    }
}

/// One compiled exclusion or temporal key of a model.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    /// The object's full name (the constraint's name).
    pub(crate) full_name: String,
    /// `exclusion '<name>'` or `key '<name>'`.
    pub(crate) what: String,
    /// Which kind of object this is.
    pub(crate) kind: ObjectKind,
    /// The canonical constraint definition (stored in the comment).
    pub(crate) definition: String,
    /// The period check of a typed exclusion: its name and canonical definition.
    pub(crate) check: Option<(String, String)>,
    /// Equality columns inside a GiST index need `btree_gist`.
    pub(crate) needs_btree_gist: bool,
    /// The diagnostic query for the id pairs of existing rows that would violate the object
    /// ([`erp_sql::conflict::conflict_pairs_query`]).
    pub(crate) pairs_query: String,
    /// Pieces the catalog's own definition (`pg_get_constraintdef`, double quotes removed) of an
    /// exclusion must contain; a hand-altered constraint that kept the ORM's comment lacks one
    /// (review D2). A temporal key is compared whole.
    fragments: Vec<String>,
    /// The exclusion has a `WHERE` filter.
    filtered: bool,
    /// How many `WITH <operator>` elements the definition has: the equality columns and the
    /// period.
    with_count: usize,
    /// The declared name of the object (`_no_double_booking`).
    pub(crate) name: &'static str,
}

impl Compiled {
    /// Whether the catalog's definition of the constraint is the one this object compiles to.
    /// PostgreSQL prints a definition in its own form (casts added, quotes dropped), so an
    /// exclusion is compared by its pieces, a temporal key by its text.
    fn catalog_matches(&self, catalog_def: &str) -> bool {
        let def = catalog_def.replace('"', "");
        match self.kind {
            ObjectKind::TemporalKey => def == self.definition.replace('"', ""),
            ObjectKind::Exclusion => {
                exclusion_matches(&def, &self.fragments, self.with_count, self.filtered)
            }
        }
    }
}

/// Whether the catalog's `def` (double quotes removed) of an exclusion has every element of
/// `fragments` as a whole element (after the opening parenthesis or a comma, so `id WITH =` is not
/// found inside `partner_id WITH =`), exactly `with_count` elements, and a `WHERE` iff `filtered`.
/// A changed filter text is not detected here: the comment drives that (design §11.1).
fn exclusion_matches(def: &str, fragments: &[String], with_count: usize, filtered: bool) -> bool {
    fragments.iter().all(|f| {
        if f.starts_with("EXCLUDE") || f.starts_with("WITH") {
            def.contains(f.as_str())
        } else {
            def.contains(&format!("({f}")) || def.contains(&format!(", {f}"))
        }
    }) && element_list(def).is_some_and(|list| with_elements(list) == with_count)
        && def.contains(" WHERE ") == filtered
}

/// The text between the parentheses of `EXCLUDE USING gist (...)`: the element list, without a
/// `WHERE` filter or a storage `WITH (...)` after it. Quoted text is skipped, so a parenthesis in
/// a bounds literal does not end it.
fn element_list(def: &str) -> Option<&str> {
    let open = "EXCLUDE USING gist (";
    let start = def.find(open)? + open.len();
    let mut depth = 1_u32;
    let mut quoted = false;
    for (i, c) in def[start..].char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(&def[start..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// How many `<expression> WITH <operator>` elements `list` has, outside quoted text.
fn with_elements(list: &str) -> usize {
    let mut quoted = false;
    let mut count = 0;
    let mut rest = list;
    while let Some(c) = rest.chars().next() {
        if c == '\'' {
            quoted = !quoted;
        }
        if !quoted && rest.starts_with(" WITH ") {
            count += 1;
        }
        rest = &rest[c.len_utf8()..];
    }
    count
}

impl ObjectKind {
    /// How a conflict message names this kind of object.
    pub(crate) const fn conflict_object(self) -> ConflictObject {
        match self {
            Self::Exclusion => ConflictObject::Exclusion,
            Self::TemporalKey => ConflictObject::Key,
        }
    }
}

/// The compiled exclusions and temporal keys of a registry, per model.
#[derive(Debug, Default)]
pub(crate) struct Exclusions {
    by_model: FxHashMap<ModelId, Vec<Compiled>>,
    /// The compiled constant filters of typed indexes, by model and index full name.
    index_filters: FxHashMap<(ModelId, String), String>,
}

impl Exclusions {
    /// Compile the exclusions and temporal keys of every model.
    ///
    /// # Errors
    ///
    /// `Programming` for a filter that is not a constant domain over the model's own stored
    /// fields: `<object> on '<model>': filter <domain>: <reason>` (design §10).
    pub(crate) fn build(registry: &Registry) -> Result<Self> {
        let mut by_model: FxHashMap<ModelId, Vec<Compiled>> = FxHashMap::default();
        let mut index_filters = FxHashMap::default();
        for model in registry.models() {
            for object in &model.table_objects {
                if let TableObjectKind::TypedIndex(index) = object.kind
                    && let Some(text) = index.filter
                {
                    let predicate =
                        erp_sql::constant_filter::compile(text, model).map_err(|reason| {
                            Error::programming(format!(
                                "index '{}' on '{}': filter {text}: {reason}",
                                object.name, model.name
                            ))
                        })?;
                    index_filters.insert((model.id, object.full_name.clone()), predicate);
                }
                if let Some(compiled) = compile(model, object)? {
                    by_model.entry(model.id).or_default().push(compiled);
                }
            }
        }
        Ok(Self {
            by_model,
            index_filters,
        })
    }

    /// The compiled constant filter (an SQL predicate) of the typed index `full_name`: `None`
    /// when `declared` has no filter.
    ///
    /// # Errors
    ///
    /// `Programming` when `declared` has a filter this registry never compiled: the index would
    /// be created without its `WHERE`.
    pub(crate) fn index_filter(
        &self,
        model: ModelId,
        full_name: &str,
        declared: &erp_registry::IndexDecl,
    ) -> Result<Option<&str>> {
        let Some(text) = declared.filter else {
            return Ok(None);
        };
        self.index_filters
            .get(&(model, full_name.to_owned()))
            .map(|predicate| Some(predicate.as_str()))
            .ok_or_else(|| {
                Error::programming(format!(
                    "index {full_name}: the filter {text} was not compiled when the ORM was \
                     built; refusing to create the index without it"
                ))
            })
    }

    fn of(&self, model: ModelId) -> &[Compiled] {
        self.by_model.get(&model).map_or(&[], Vec::as_slice)
    }

    /// The compiled objects of `model`, in declaration order.
    pub(crate) fn compiled(&self, model: ModelId) -> &[Compiled] {
        self.of(model)
    }

    /// `(object kind, pairs query)` of the exclusion or temporal key `full_name` of `model`: how
    /// a schema-sync conflict names the object and finds the rows that conflict.
    pub(crate) fn conflict_probe(
        &self,
        model: ModelId,
        full_name: &str,
    ) -> Option<(ConflictObject, &str)> {
        self.of(model)
            .iter()
            .find(|c| c.full_name == full_name)
            .map(|c| (c.kind.conflict_object(), c.pairs_query.as_str()))
    }
}

fn compile(model: &ModelSpec, object: &TableObject) -> Result<Option<Compiled>> {
    let Some(what) = describe_object(object) else {
        return Ok(None);
    };
    let fail = |reason: String| Error::programming(format!("{what} on '{}': {reason}", model.name));
    let (kind, definition, check, needs_btree_gist, pairs_query, fragments, filtered, with_count) =
        match object.kind {
            TableObjectKind::Exclusion(decl) => {
                let filter = match decl.filter {
                    None => None,
                    Some(text) => Some(
                        erp_sql::constant_filter::compile_filter(text, model)
                            .map_err(|reason| fail(format!("filter {text}: {reason}")))?,
                    ),
                };
                let definition =
                    exclusion_definition(model, &decl, filter.as_ref().map(|f| f.as_sql()))
                        .map_err(&fail)?;
                let check = (
                    period_check_name(&object.full_name),
                    period_check_definition(&decl),
                );
                let period = compiled_period(model, &decl).map_err(&fail)?;
                let pairs_query = conflict_pairs_query(
                    &model.table,
                    decl.equal,
                    KeyMatch::Equal,
                    Some(&period),
                    filter.as_ref(),
                );
                // the period's closing parenthesis is left out: the catalog prints the bounds
                // literal with a cast
                let period_head = period.as_sql().replace('"', "");
                let mut fragments = vec!["EXCLUDE USING gist (".to_owned(), "WITH &&".to_owned()];
                fragments.push(period_head.trim_end_matches(')').to_owned());
                fragments.extend(decl.equal.iter().map(|f| format!("{f} WITH =")));
                (
                    ObjectKind::Exclusion,
                    definition,
                    Some(check),
                    !decl.equal.is_empty(),
                    pairs_query,
                    fragments,
                    filter.is_some(),
                    decl.equal.len() + 1,
                )
            }
            TableObjectKind::TemporalKey(decl) => {
                let range = compiled_range(&decl);
                let pairs_query = conflict_pairs_query(
                    &model.table,
                    decl.fields,
                    KeyMatch::Equal,
                    Some(&range),
                    None,
                );
                (
                    ObjectKind::TemporalKey,
                    temporal_key_definition(&decl),
                    None,
                    !decl.fields.is_empty(),
                    pairs_query,
                    Vec::new(),
                    false,
                    0,
                )
            }
            TableObjectKind::Constraint { .. }
            | TableObjectKind::Index { .. }
            | TableObjectKind::Key(_)
            | TableObjectKind::UniqueIndex { .. }
            | TableObjectKind::TypedIndex(_)
            | TableObjectKind::SignalTrigger(_)
            | TableObjectKind::FollowDelete(_) => return Ok(None),
        };
    Ok(Some(Compiled {
        full_name: object.full_name.clone(),
        what,
        kind,
        definition,
        check,
        needs_btree_gist,
        pairs_query,
        fragments,
        filtered,
        with_count,
        name: object.name,
    }))
}

/// The `(a.id, b.id)` pairs of the rows a [`erp_sql::conflict::conflict_pairs_query`] returned;
/// the one reader every conflict diagnostic uses.
pub(crate) fn read_conflict_pairs(
    rows: &[tokio_postgres::Row],
) -> std::result::Result<Vec<(i64, i64)>, tokio_postgres::Error> {
    rows.iter()
        .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
        .collect()
}

/// `(definition, comment, "type:columns")` of the constraints of one table, as schema sync reads
/// them from the catalog.
pub(crate) type CatalogConstraints = FxHashMap<String, (String, Option<String>, String)>;

/// The change schema sync makes to bring one exclusion or temporal key in line with its
/// declaration.
pub(crate) struct ObjectSync {
    /// `exclusion '<name>'` or `key '<name>'`, as messages name the object.
    pub(crate) what: String,
    /// Which kind of object it is.
    pub(crate) kind: ObjectKind,
    /// The statements, in order: the period check of a typed exclusion (added `NOT VALID`, then
    /// validated), then the constraint, each (re-)created when its comment does not hold the
    /// canonical definition.
    pub(crate) statements: Vec<String>,
}

/// PostgreSQL's name for a `pg_constraint.contype` letter.
fn contype_name(contype: char) -> &'static str {
    match contype {
        'p' => "primary key",
        'f' => "foreign key",
        'u' => "unique constraint",
        'c' => "check constraint",
        'x' => "exclusion constraint",
        'n' => "not-null constraint",
        't' => "constraint trigger",
        _ => "constraint of another type",
    }
}

/// What brings the exclusion or temporal key `object` of `spec` in line with its declaration:
/// `None` when the catalog already holds the canonical definitions.
///
/// The caller decides where the statements run: in the schema transaction, or, on a table above
/// `large_table_rows`, not at all (an exclusion has no online build, design §11).
///
/// # Errors
///
/// `Programming` when the object was not compiled with the ORM (a registry and ORM mismatch), or
/// when the table has a constraint of the object's name that the ORM did not create for it.
pub(crate) fn sync_statements(
    exclusions: &Exclusions,
    spec: &ModelSpec,
    object: &TableObject,
    constraints: Option<&CatalogConstraints>,
) -> Result<Option<ObjectSync>> {
    let compiled = exclusions
        .of(spec.id)
        .iter()
        .find(|c| c.full_name == object.full_name)
        .ok_or_else(|| {
            Error::programming(format!(
                "table object '{}' of '{}' was not compiled with the ORM",
                object.full_name, spec.name
            ))
        })?;
    // An existing constraint of the name is replaced only when the ORM made it: it carries the
    // canonical definition as its comment and has the contype the object compiles to. Anything
    // else (a primary or foreign key, a hand-made constraint) is never dropped.
    let current = |name: &str,
                   contype: char,
                   prefix: &str|
     -> Result<Option<&(String, Option<String>, String)>> {
        let Some(existing) = constraints.and_then(|c| c.get(name)) else {
            return Ok(None);
        };
        // the ORM's comment is the canonical definition, which starts with the same text as
        // the catalog's own definition of the constraint
        let ours = existing.2.starts_with(contype)
            && existing.0.starts_with(prefix)
            && existing.1.as_deref().is_some_and(|c| c.starts_with(prefix));
        // a constraint carrying exactly this object's canonical definition as its comment is the
        // ORM's even when its type or definition was changed by hand since (review D2): it is
        // compared below and re-created
        if ours || existing.1.as_deref() == Some(compiled.definition.as_str()) {
            return Ok(Some(existing));
        }
        let found = existing.2.chars().next().map_or("unknown", contype_name);
        Err(Error::programming(format!(
            "{} on '{}': the table '{}' already has a constraint named '{name}' ({found}) that \
             the ORM did not create for this object; it is not replaced, rename the object",
            compiled.what, spec.name, spec.table
        )))
    };
    let mut out = Vec::new();
    if let Some((name, definition)) = &compiled.check {
        match current(name, 'c', "CHECK (")? {
            Some((_, comment, _)) if comment.as_deref() == Some(definition.as_str()) => {}
            existing => {
                if existing.is_some() {
                    out.push(erp_sql::ddl::drop_constraint(&spec.table, name));
                }
                out.extend(add_check(&spec.table, name, definition));
            }
        }
    }
    let (contype, prefix) = match compiled.kind {
        ObjectKind::Exclusion => ('x', "EXCLUDE USING gist ("),
        ObjectKind::TemporalKey => ('u', "UNIQUE ("),
    };
    match current(&compiled.full_name, contype, prefix)? {
        // up to date: the comment holds the declaration, the type is the object's and the
        // catalog's own definition agrees (review D2)
        Some((def, comment, kind))
            if comment.as_deref() == Some(compiled.definition.as_str())
                && kind.starts_with(contype)
                && compiled.catalog_matches(def) => {}
        existing => {
            if existing.is_some() {
                out.push(erp_sql::ddl::drop_constraint(
                    &spec.table,
                    &compiled.full_name,
                ));
            }
            out.extend(erp_sql::ddl::add_constraint(
                &spec.table,
                &compiled.full_name,
                &compiled.definition,
            ));
        }
    }
    Ok((!out.is_empty()).then(|| ObjectSync {
        what: compiled.what.clone(),
        kind: compiled.kind,
        statements: out,
    }))
}

/// The install error for an exclusion or temporal key on a table above `large_table_rows`
/// (design §10, §11): building it takes `ACCESS EXCLUSIVE` for the whole GiST build and has no
/// online form, so `erp db build-object` takes that window on purpose instead.
pub(crate) fn large_table_refusal(
    model: &str,
    table: &str,
    object: &str,
    what: &str,
    kind: ObjectKind,
    limit: u64,
) -> Error {
    let kind = kind.phrase();
    Error::new(
        ErrorKind::UserError,
        format!(
            "cannot add {what} to '{model}' during the install: table '{table}' has more than \
             {limit} rows and {kind} has no online build. Run `erp db build-object -d <db> \
             --model {model} --object {object}` in a maintenance window (it holds ACCESS \
             EXCLUSIVE on the table for the build), then retry the install"
        ),
    )
}

/// An extension schema sync installs: the extension, the first model needing it and the object
/// that does (for the install messages).
pub(crate) type ExtensionNeed = (PgExtension, &'static str, String);

impl Transaction {
    /// The extensions the models of `specs` need that schema sync installs and the database lacks
    /// (`installed`, from the catalog): `btree_gist` for exclusions, temporal keys with equality
    /// columns and typed `gist` indexes, and what models list in `extensions = [...]`. The first
    /// model and object needing each, in registry order. `pg_trgm` is installed only when a model
    /// lists it; the extension gate (`schema_index::check_extensions`) counts what this returns as
    /// installed, so it is the single place that decides what a missing extension is.
    pub(crate) fn extension_needs(
        &self,
        specs: &[&ModelSpec],
        installed: &FxHashSet<String>,
    ) -> Vec<ExtensionNeed> {
        let mut needed: Vec<ExtensionNeed> = Vec::new();
        let mut note = |ext: PgExtension, model: &'static str, what: String| {
            if !installed.contains(ext.name()) && !needed.iter().any(|(e, _, _)| *e == ext) {
                needed.push((ext, model, what));
            }
        };
        for spec in specs {
            for compiled in self.shared.exclusions.of(spec.id) {
                if compiled.needs_btree_gist {
                    note(PgExtension::BtreeGist, spec.name, compiled.what.clone());
                }
            }
            for object in &spec.table_objects {
                if let TableObjectKind::TypedIndex(index) = object.kind
                    && index.method == IndexMethod::Gist
                {
                    note(
                        PgExtension::BtreeGist,
                        spec.name,
                        format!("index '{}'", object.name),
                    );
                }
            }
            for decl in spec.mro.iter().rev() {
                for ext in decl.extensions {
                    note(
                        *ext,
                        spec.name,
                        format!("extensions = [\"{}\"]", ext.name()),
                    );
                }
            }
        }
        needed
    }

    /// Install `needed` ([`Self::extension_needs`]; `installed` is updated). Each runs as
    /// `CREATE EXTENSION IF NOT EXISTS` in the schema transaction, before the objects that need it.
    ///
    /// # Errors
    ///
    /// `Database`, naming the model, the extension and the object (design §10), when the extension
    /// is not available on the server or cannot be installed (no `CREATE` privilege on the
    /// database, for example).
    pub(crate) async fn install_extensions(
        &mut self,
        needed: Vec<ExtensionNeed>,
        installed: &mut FxHashSet<String>,
    ) -> Result<()> {
        for (ext, model, what) in needed {
            let name = ext.name();
            let available = self
                .query(
                    "SELECT 1 FROM pg_available_extensions WHERE name = $1",
                    &[&name],
                )
                .await?;
            if available.is_empty() {
                return Err(not_available(model, ext, &what));
            }
            if let Err(error) = self.batch(&create_extension(ext)).await {
                return Err(not_installable(model, ext, &what, error));
            }
            installed.insert(name.to_owned());
        }
        Ok(())
    }
}

/// The install error for an extension the server does not have (design §10).
pub(crate) fn not_available(model: &str, ext: PgExtension, what: &str) -> Error {
    Error::new(
        ErrorKind::Database,
        format!(
            "model '{model}' needs the PostgreSQL extension {} for {what}; it is not available on \
             this server",
            ext.name()
        ),
    )
}

/// The install error for an extension `CREATE EXTENSION` refused (design §10), keeping the
/// server's error as the source.
pub(crate) fn not_installable(model: &str, ext: PgExtension, what: &str, error: Error) -> Error {
    let name = ext.name();
    let sqlstate = error.sqlstate().map(str::to_owned);
    Error::new(
        ErrorKind::Database,
        format!(
            "model '{model}' needs the PostgreSQL extension {name} for {what}; it could not be \
             installed: {}; ask the database administrator to run CREATE EXTENSION {name}",
            error.message()
        ),
    )
    .with_sqlstate(sqlstate)
    .with_source(error)
}

/// The periods of the typed exclusions of `spec`: their two fields and bounds. The registry
/// refuses an exclusion whose period fields do not exist (`erp_registry`'s `check_model`), so no
/// exclusion is left out here.
fn periods(spec: &ModelSpec) -> impl Iterator<Item = (&FieldSpec, &FieldSpec, RangeBounds)> {
    spec.table_objects.iter().filter_map(|object| {
        let TableObjectKind::Exclusion(decl) = object.kind else {
            return None;
        };
        Some((
            spec.field(decl.period.0)?,
            spec.field(decl.period.1)?,
            decl.bounds,
        ))
    })
}

/// Whether two period ends make an empty or inverted period (`None` or NULL: unbounded, never).
/// Ends of different kinds cannot occur: the registry requires two date or two datetime fields and
/// every cell is validated against its field before it gets here.
fn empty_period(from: Option<&Cell>, to: Option<&Cell>, bounds: RangeBounds) -> bool {
    match (from, to) {
        (Some(Cell::Date(a)), Some(Cell::Date(b))) => bounds.is_empty(a, b),
        (Some(Cell::DateTime(a)), Some(Cell::DateTime(b))) => bounds.is_empty(a, b),
        _ => false,
    }
}

/// The validation error for an empty or inverted period (design §10): `'<from label>' must be
/// before '<to label>' on '<description>'` (`must not be after` for `[]`).
fn period_error(spec: &ModelSpec, from: &FieldSpec, to: &FieldSpec, bounds: RangeBounds) -> Error {
    Error::validation(format!(
        "The operation cannot be completed: '{}' must {} '{}' on '{}'",
        from.string,
        bounds.order_words(),
        to.string,
        spec.description
    ))
}

/// Refuse the rows about to be inserted whose period of a typed exclusion is empty or inverted
/// (indexed by field, as `create` builds them), before the `INSERT`.
pub(crate) fn check_rows(spec: &ModelSpec, rows: &[Vec<Option<Cell>>]) -> Result<()> {
    for (from, to, bounds) in periods(spec) {
        let (a, b) = (from.id.index(), to.id.index());
        for row in rows {
            let (Some(from_cell), Some(to_cell)) = (row.get(a), row.get(b)) else {
                return Err(Error::programming(format!(
                    "internal error: a row of '{}' has {} cells, shorter than the period fields \
                     '{}' (column {a}) and '{}' (column {b})",
                    spec.name,
                    row.len(),
                    from.name,
                    to.name
                )));
            };
            if empty_period(from_cell.as_ref(), to_cell.as_ref(), bounds) {
                return Err(period_error(spec, from, to, bounds));
            }
        }
    }
    Ok(())
}

/// Refuse a write that makes the period of a typed exclusion empty or inverted, for the records
/// whose other end is the written value or already cached. Nothing is fetched: when the other end
/// is not cached, the generated `CHECK` refuses the row at flush with the same message.
pub(crate) fn check_write(
    tx: &Transaction,
    spec: &ModelSpec,
    ids: &[RecId],
    items: &[(FieldId, Cell, bool)],
) -> Result<()> {
    for (from, to, bounds) in periods(spec) {
        let written = |f: FieldId| items.iter().find(|(id, _, _)| *id == f).map(|(_, c, _)| c);
        let (new_from, new_to) = (written(from.id), written(to.id));
        if new_from.is_none() && new_to.is_none() {
            continue;
        }
        for id in ids {
            let end = |new: Option<&Cell>, field: &FieldSpec| {
                new.cloned()
                    .or_else(|| tx.cache.get(spec.id, field.id, *id))
            };
            let (a, b) = (end(new_from, from), end(new_to, to));
            if empty_period(a.as_ref(), b.as_ref(), bounds) {
                return Err(period_error(spec, from, to, bounds));
            }
        }
    }
    Ok(())
}

/// The message of a violated period check (`23514` on `constraint`), when `constraint` is the
/// period check of an exclusion of `model`: the same field-named text as the check before the
/// statement.
pub(crate) fn period_check_message(model: &ModelSpec, constraint: &str) -> Option<String> {
    model.table_objects.iter().find_map(|object| {
        let TableObjectKind::Exclusion(decl) = object.kind else {
            return None;
        };
        if period_check_name(&object.full_name) != constraint {
            return None;
        }
        let (from, to) = (model.field(decl.period.0)?, model.field(decl.period.1)?);
        Some(
            period_error(model, from, to, decl.bounds)
                .message()
                .to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// development clusters cannot show: they ship every extension a model may declare)
    #[test]
    fn missing_extensions_are_named_with_the_model_and_object() {
        let err = not_available(
            "scale.booking",
            PgExtension::BtreeGist,
            "exclusion '_no_double_booking'",
        );
        assert_eq!(err.kind(), ErrorKind::Database);
        assert_eq!(
            err.message(),
            "model 'scale.booking' needs the PostgreSQL extension btree_gist for exclusion \
             '_no_double_booking'; it is not available on this server"
        );
        let server = Error::new(ErrorKind::Database, "permission denied")
            .with_sqlstate(Some("42501".to_owned()));
        let err = not_installable(
            "scale.raw",
            PgExtension::PgTrgm,
            "extensions = [\"pg_trgm\"]",
            server,
        );
        assert_eq!(err.kind(), ErrorKind::Database);
        assert_eq!(err.sqlstate(), Some("42501"));
        assert_eq!(
            err.message(),
            "model 'scale.raw' needs the PostgreSQL extension pg_trgm for extensions = \
             [\"pg_trgm\"]; it could not be installed: permission denied; ask the database \
             administrator to run CREATE EXTENSION pg_trgm"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    /// A declared filter that was never compiled is an error, not an index without `WHERE`.
    #[test]
    fn an_uncompiled_index_filter_is_an_error() {
        static ADDON: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
            name: "scale",
            depends: &["base"],
            exact_only: false,
        };
        static DECL: std::sync::LazyLock<erp_registry::ModelDecl> =
            std::sync::LazyLock::new(|| erp_registry::ModelDecl::new("scale", "scale.x"));
        let registry = erp_registry::RegistryBuilder::new([&ADDON])
            .with_decls([&*DECL])
            .build()
            .unwrap();
        let model = registry.model("scale.x").unwrap().id;
        let exclusions = Exclusions::default();
        let plain = erp_registry::IndexDecl::new(&["a"]);
        assert_eq!(exclusions.index_filter(model, "t_i", &plain).unwrap(), None);
        let declared = plain.filter("[('a', '=', 1)]");
        let err = exclusions
            .index_filter(model, "t_i", &declared)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "index t_i: the filter [('a', '=', 1)] was not compiled when the ORM was built; \
             refusing to create the index without it"
        );
    }

    /// Review R7: a row shorter than the field list is an internal error, never a pass.
    #[test]
    fn a_short_row_is_an_internal_error() {
        use erp_registry::{
            AddonDescriptor, ExclusionDecl, FieldDecl, ModelDecl, RegistryBuilder, TableObjectDecl,
        };
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "test_check_rows",
            depends: &["base"],
            exact_only: false,
        };
        static FIELDS: [FieldDecl; 2] = [FieldDecl::date("arrival"), FieldDecl::date("departure")];
        static OBJECTS: [TableObjectDecl; 1] = [TableObjectDecl {
            name: "_rule",
            kind: TableObjectKind::Exclusion(ExclusionDecl::new(
                ("arrival", "departure"),
                RangeBounds::HalfOpen,
                "taken",
            )),
        }];
        let decl: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_check_rows", "excl.rows")
                .fields(&FIELDS)
                .table_objects(&OBJECTS),
        ));
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([decl])
            .build()
            .unwrap();
        let spec = registry.model("excl.rows").unwrap();
        let (a, b) = (
            spec.field("arrival").unwrap().id.index(),
            spec.field("departure").unwrap().id.index(),
        );
        let err = check_rows(spec, &[vec![]]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "internal error: a row of 'excl.rows' has 0 cells, shorter than the period \
                 fields 'arrival' (column {a}) and 'departure' (column {b})"
            )
        );
        // a full row of NULL ends passes
        let full = vec![None; a.max(b) + 1];
        check_rows(spec, &[full]).unwrap();
    }

    fn pieces(equal: &[&str]) -> Vec<String> {
        let mut out = vec!["EXCLUDE USING gist (".to_owned(), "WITH &&".to_owned()];
        out.push("tsrange(start, stop, '[)'".to_owned());
        out.extend(equal.iter().map(|f| format!("{f} WITH =")));
        out
    }

    /// Whole elements only, and exactly the declared number of them (review SF-S3, DB6).
    #[test]
    fn exclusion_elements_are_matched_whole_and_counted() {
        let ok = "EXCLUDE USING gist (partner_id WITH =, tsrange(start, stop, '[)'::text) WITH &&)";
        assert!(exclusion_matches(ok, &pieces(&["partner_id"]), 2, false));
        // `id WITH =` is not found inside `partner_id WITH =`
        assert!(!exclusion_matches(ok, &pieces(&["id"]), 2, false));
        // an element added by hand
        let extra = "EXCLUDE USING gist (partner_id WITH =, room WITH =, tsrange(start, stop, '[)'::text) WITH &&)";
        assert!(!exclusion_matches(
            extra,
            &pieces(&["partner_id"]),
            2,
            false
        ));
        // a `WITH` in the filter's text or in a storage clause is not an element
        let literal_with = "EXCLUDE USING gist (partner_id WITH =, tsrange(start, stop, '[)'::text) WITH &&) WHERE ((state)::text <> 'a WITH b'::text)";
        assert!(exclusion_matches(
            literal_with,
            &pieces(&["partner_id"]),
            2,
            true
        ));
        let storage = "EXCLUDE USING gist (partner_id WITH =, tsrange(start, stop, '[)'::text) WITH &&) WITH (fillfactor='70')";
        assert!(exclusion_matches(
            storage,
            &pieces(&["partner_id"]),
            2,
            false
        ));
        // a filter that is not declared, and one that is declared but missing
        let filtered = format!("{ok} WHERE ((state <> 'cancel'::text))");
        assert!(!exclusion_matches(
            &filtered,
            &pieces(&["partner_id"]),
            2,
            false
        ));
        assert!(!exclusion_matches(ok, &pieces(&["partner_id"]), 2, true));
        assert!(exclusion_matches(
            &filtered,
            &pieces(&["partner_id"]),
            2,
            true
        ));
    }
}
