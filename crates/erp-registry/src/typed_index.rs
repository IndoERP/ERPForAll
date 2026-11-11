//! A typed index names fields, never SQL, so every source may declare it, `models/*.toml` of a
//! database-stored package included (design §2). The registry checks it when it builds the model:
//! the fields exist and are stored in the model's table, each key field suits the access method,
//! `include` is used only where PostgreSQL supports it and never repeats a key field.
//!
//! ```
//! use erp_registry::{IndexDecl, IndexMethod, TableObjectDecl};
//!
//! // `index(name = "_open_by_date", fields = ["state", "start"], include = ["resource_id"])`
//! const OPEN_BY_DATE: TableObjectDecl = TableObjectDecl::index(
//!     "_open_by_date",
//!     IndexDecl::new(&["state", "start"]).include(&["resource_id"]),
//! );
//! const BY_DAY: TableObjectDecl =
//!     TableObjectDecl::index("_by_day", IndexDecl::new(&["date"]).method(IndexMethod::Brin));
//! # assert_eq!(BY_DAY.name, "_by_day");
//! # let _ = OPEN_BY_DATE;
//! ```

use erp_fields::{Error, FieldType, IndexKind, Result};
use rustc_hash::FxHashMap;

use crate::decl::{TableObjectDecl, TableObjectKind};
use crate::spec::{FieldId, FieldSpec, TableObject};

/// Most columns of one index, key and `INCLUDE` columns together (PostgreSQL's `INDEX_MAX_KEYS`).
pub const MAX_INDEX_COLUMNS: usize = 32;

/// The access method of a typed model-level index (`method = "..."`, design §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexMethod {
    /// `btree` (the default): equality, ranges and ordering; `include` allowed.
    Btree,
    /// `brin`: block ranges, for `date`, `datetime` and `integer` fields whose values follow the
    /// insert order.
    Brin,
    /// `gin` with `jsonb_path_ops`, for `json` fields (`@>`, `@?`, `@@`). It is that one operator
    /// class, not a general GIN (no `jsonb_ops` key-existence operators, no arrays or full text).
    Gin,
    /// `gist`; scalar columns need the `btree_gist` extension; `include` allowed.
    Gist,
    /// `gin` with `gin_trgm_ops` (needs `pg_trgm`), for `char`, `text` and `html` fields; the
    /// columns are folded like `ilike` conditions when accent folding is indexable.
    Trigram,
}

impl IndexMethod {
    /// The name used in declarations (`method = "brin"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Btree => "btree",
            Self::Brin => "brin",
            Self::Gin => "gin",
            Self::Gist => "gist",
            Self::Trigram => "trigram",
        }
    }

    /// The method named `name` in a declaration.
    ///
    /// ```
    /// use erp_registry::IndexMethod;
    /// assert_eq!(IndexMethod::from_name("gist"), Some(IndexMethod::Gist));
    /// assert_eq!(IndexMethod::from_name("hash"), None);
    /// ```
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "btree" => Self::Btree,
            "brin" => Self::Brin,
            "gin" => Self::Gin,
            "gist" => Self::Gist,
            "trigram" => Self::Trigram,
            _ => return None,
        })
    }

    /// The PostgreSQL access method (`USING <method>`, `pg_am.amname`).
    pub fn access_method(self) -> &'static str {
        match self {
            Self::Btree => "btree",
            Self::Brin => "brin",
            Self::Gin | Self::Trigram => "gin",
            Self::Gist => "gist",
        }
    }

    /// The operator class of every key column, when not the column type's default one.
    pub fn opclass(self) -> Option<&'static str> {
        match self {
            Self::Gin => Some("jsonb_path_ops"),
            Self::Trigram => Some("gin_trgm_ops"),
            Self::Btree | Self::Brin | Self::Gist => None,
        }
    }

    /// The PostgreSQL extension the method needs for the field types it accepts.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Trigram => Some("pg_trgm"),
            Self::Gist => Some("btree_gist"),
            Self::Btree | Self::Brin | Self::Gin => None,
        }
    }

    /// True when the method supports `INCLUDE` columns (design §6.2: btree and gist).
    pub fn allows_include(self) -> bool {
        match self {
            Self::Btree | Self::Gist => true,
            Self::Brin | Self::Gin | Self::Trigram => false,
        }
    }

    pub fn field_types(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Btree => None,
            // the same lists as `IndexKind::field_types` (pinned by `tests/index_kinds.rs`)
            Self::Brin => Some(&["date", "datetime", "integer"]),
            Self::Gin => Some(&["json"]),
            // the types `btree_gist` has operator classes for (no `jsonb`)
            Self::Gist => Some(&[
                "boolean",
                "integer",
                "float",
                "monetary",
                "char",
                "text",
                "html",
                "selection",
                "date",
                "datetime",
                "many2one",
                "many2one_reference",
                "reference",
                // range columns have their own GiST operator class (no `btree_gist` needed)
                "date_range",
                "datetime_range",
            ]),
            Self::Trigram => Some(&["char", "text", "html"]),
        }
    }
}

/// A typed model-level index: key fields, an access method and covering (`INCLUDE`) fields
/// (`index(name, fields, method, include)` in `#[model]`/`#[extend]`, `[[model.index]]` in
/// `models/*.toml`). The name is the [`TableObjectDecl`]'s.
///
/// `filter` is a constant domain (design §5.4, `[('state', '=', 'open')]`) that makes the index a
/// partial index; the ORM compiles it to a literal predicate when it is built
/// (`erp_sql::constant_filter`), so a filter over unknown fields or with a non-literal operand
/// fails there, naming the index and the model.
///
/// # Errors
///
/// Nothing is checked when the value is built. The registry build fails with `Programming` when: a
/// field does not exist or has no column in the model's table (`id` counts as a column); a field
/// is listed twice, or an `include` field repeats a key field; a key field's type does not suit
/// the method (`brin`: date, datetime, integer; `gin`: json; `trigram`: char, text, html; `gist`:
/// the scalar and range types; a `company_dependent` field counts as `json`); `include` is used
/// with a method other than `btree` or `gist`; the index has no field or more than
/// [`MAX_INDEX_COLUMNS`] columns; the model composes to more than [`MAX_INDEXES_PER_MODEL`] typed
/// indexes; one declaration repeats a name. The ORM build fails with `Programming` when `filter` is
/// not a constant domain over the model's own stored fields. Schema sync fails with `Database` when
/// the method's extension is missing and is not installed by the sync.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct IndexDecl {
    /// Key fields, in index order.
    pub fields: &'static [&'static str],
    /// Access method.
    pub method: IndexMethod,
    /// Covering fields stored in the index but not part of its key (`INCLUDE`).
    pub include: &'static [&'static str],
    /// Constant domain in list syntax: only rows matching it are indexed (`WHERE`).
    pub filter: Option<&'static str>,
}

impl IndexDecl {
    /// A btree index over `fields`, without covering fields.
    pub const fn new(fields: &'static [&'static str]) -> Self {
        Self {
            fields,
            method: IndexMethod::Btree,
            include: &[],
            filter: None,
        }
    }

    /// Set the access method.
    #[must_use]
    pub const fn method(mut self, method: IndexMethod) -> Self {
        self.method = method;
        self
    }

    /// Index only the rows matching the constant domain `domain`.
    #[must_use]
    pub const fn filter(mut self, domain: &'static str) -> Self {
        self.filter = Some(domain);
        self
    }

    /// Set the covering (`INCLUDE`) fields.
    #[must_use]
    pub const fn include(mut self, fields: &'static [&'static str]) -> Self {
        self.include = fields;
        self
    }
}

impl TableObjectDecl {
    /// A typed index named `name` (`_` first, like every table object).
    pub const fn index(name: &'static str, index: IndexDecl) -> Self {
        Self {
            name,
            kind: TableObjectKind::TypedIndex(index),
        }
    }
}

/// Most typed indexes one runtime definition ([`crate::ModelDef`]) may declare.
pub const MAX_INDEXES_PER_MODEL: usize = 64;

/// A typed index declared at runtime (`[[model.index]]` in `models/*.toml`), owned; it becomes an
/// [`IndexDecl`] table object when its [`crate::ModelDef`] is interned.
///
/// ```
/// use erp_registry::{IndexDef, IndexMethod, ModelDef};
/// let booking = ModelDef::extension("my_rules", "resource.booking")
///     .index(IndexDef::new("_by_date", ["date"]).method(IndexMethod::Brin));
/// assert_eq!(booking.indexes.len(), 1);
/// ```
///
/// # Errors
///
/// Building the value checks nothing. [`crate::ModelDef::check_limits`] (run when the definition is
/// interned) fails with `Validation` for a name without the leading `_`, a repeated name, a name
/// or field name over the name limit, more than [`MAX_INDEXES_PER_MODEL`] indexes, more than
/// [`MAX_INDEX_COLUMNS`] columns, or a `filter` over `MAX_TEXT_BYTES`; the registry build then
/// applies the checks listed on [`IndexDecl`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct IndexDef {
    /// Attribute name, `_` first (`_by_date`).
    pub name: String,
    /// Key fields, in index order.
    pub fields: Vec<String>,
    /// Access method.
    pub method: IndexMethod,
    /// Covering (`INCLUDE`) fields.
    pub include: Vec<String>,
    /// Constant domain in list syntax: only rows matching it are indexed.
    pub filter: Option<String>,
}

impl IndexDef {
    /// A btree index named `name` over `fields`.
    pub fn new(
        name: impl Into<String>,
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            fields: fields.into_iter().map(Into::into).collect(),
            method: IndexMethod::Btree,
            include: Vec::new(),
            filter: None,
        }
    }

    /// Set the access method.
    #[must_use]
    pub fn method(mut self, method: IndexMethod) -> Self {
        self.method = method;
        self
    }

    /// Set the covering (`INCLUDE`) fields.
    #[must_use]
    pub fn include(mut self, fields: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include = fields.into_iter().map(Into::into).collect();
        self
    }

    /// Index only the rows matching the constant domain `domain`.
    #[must_use]
    pub fn filter(mut self, domain: impl Into<String>) -> Self {
        self.filter = Some(domain.into());
        self
    }
}

/// The size limits of a runtime definition's typed indexes (names up to `max_name` bytes, at most
/// [`MAX_INDEXES_PER_MODEL`] indexes of at most [`MAX_INDEX_COLUMNS`] columns, names starting
/// with `_` and unique in the definition); field checks wait for the registry build.
pub(crate) fn check_index_defs(model: &str, indexes: &[IndexDef], max_name: usize) -> Result<()> {
    if indexes.len() > MAX_INDEXES_PER_MODEL {
        return Err(Error::validation(format!(
            "model '{model}': {} indexes, more than the limit of {MAX_INDEXES_PER_MODEL}",
            indexes.len()
        )));
    }
    for (i, index) in indexes.iter().enumerate() {
        let name = &index.name;
        let shown_name = shown(name);
        if name.len() > max_name {
            return Err(Error::validation(format!(
                "model '{model}': an index name is {} bytes long, more than the limit of \
                 {max_name}",
                name.len()
            )));
        }
        if !name.starts_with('_') {
            return Err(Error::validation(format!(
                "model '{model}': index '{shown_name}': names of SQL objects in a model start with '_'"
            )));
        }
        if indexes[..i].iter().any(|other| other.name == *name) {
            return Err(Error::validation(format!(
                "model '{model}': index '{shown_name}' is declared twice"
            )));
        }
        let columns = index.fields.len().saturating_add(index.include.len());
        if columns > MAX_INDEX_COLUMNS {
            return Err(Error::validation(format!(
                "model '{model}': index '{shown_name}' has {columns} columns; PostgreSQL allows at \
                 most {MAX_INDEX_COLUMNS}"
            )));
        }
        if let Some(field) = index
            .fields
            .iter()
            .chain(&index.include)
            .find(|f| f.len() > max_name)
        {
            return Err(Error::validation(format!(
                "model '{model}': index '{shown_name}': a field name is {} bytes long, more than the \
                 limit of {max_name}",
                field.len()
            )));
        }
    }
    Ok(())
}

/// The type an index sees for a field of type `type_name`: a `company_dependent` field is a
/// `jsonb` column whatever its type (the value per company), so index rules use `json` for it.
/// Returns that type and how messages name the field.
fn column_type_name(type_name: &str, company_dependent: bool) -> (&str, String) {
    if company_dependent {
        (
            "json",
            format!("{type_name}, company_dependent: a jsonb column"),
        )
    } else {
        (type_name, type_name.to_owned())
    }
}

/// A declared name for an error message: clipped to 64 characters and escaped, since model files
/// and overlays are untrusted text.
fn shown(name: &str) -> String {
    crate::owned::clip(name).escape_debug().to_string()
}

/// `"a"`, `"a and b"`, `"a, b and c"`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Why a field of type `type_name` may not declare `index = "<kind>"`, or `None` when it may
/// (design §6.1): the text after `field '<model>.<field>': ` in the registry's error.
///
/// ```
/// use erp_fields::IndexKind;
/// assert_eq!(
///     erp_registry::index_kind_mismatch(IndexKind::Brin, "char").as_deref(),
///     Some("index = \"brin\" is for date, datetime and integer fields (this field is char)")
/// );
/// assert_eq!(erp_registry::index_kind_mismatch(IndexKind::Gin, "json"), None);
/// ```
pub fn index_kind_mismatch(kind: IndexKind, type_name: &str) -> Option<String> {
    let types = kind.field_types()?;
    if types.contains(&type_name) {
        return None;
    }
    Some(format!(
        "index = \"{}\" is for {} fields (this field is {type_name})",
        kind.as_str(),
        and_list(types)
    ))
}

/// The registry's check of a field-level index kind against the field's type.
pub(crate) fn check_field_index(
    model: &str,
    field: &str,
    kind: IndexKind,
    ty: &FieldType,
    company_dependent: bool,
    translate: bool,
) -> Result<()> {
    if translate {
        if kind != IndexKind::Trigram {
            return Err(Error::programming(format!(
                "field '{model}.{field}': translated fields only support trigram indexes"
            )));
        }
        return Ok(());
    }
    let (lookup, described) = column_type_name(ty.type_name(), company_dependent);
    match index_kind_mismatch(kind, lookup).map(|r| {
        r.replace(
            &format!("this field is {lookup})"),
            &format!("this field is {described})"),
        )
    }) {
        None => Ok(()),
        Some(reason) => Err(Error::programming(format!(
            "field '{model}.{field}': {reason}"
        ))),
    }
}

/// Check the typed indexes of a model being built (design §6.2): every field exists and is
/// stored in the model's table, no field is listed twice, `include` is allowed by the method and
/// repeats no key field, the key fields suit the method, and the index has at most
/// [`MAX_INDEX_COLUMNS`] columns.
pub(crate) fn check_typed_indexes(
    model: &str,
    table: &str,
    fields: &[FieldSpec],
    by_name: &FxHashMap<&'static str, FieldId>,
    objects: &[TableObject],
) -> Result<()> {
    // the cap of a runtime definition is per definition; addons compose, so it holds per model
    let count = objects
        .iter()
        .filter(|o| matches!(o.kind, TableObjectKind::TypedIndex(_)))
        .count();
    if count > MAX_INDEXES_PER_MODEL {
        return Err(Error::programming(format!(
            "model '{model}': {count} typed indexes after composing every addon, more than the \
             limit of {MAX_INDEXES_PER_MODEL}"
        )));
    }
    for object in objects {
        let TableObjectKind::TypedIndex(index) = object.kind else {
            continue;
        };
        let at = || format!("index '{}' of '{model}'", shown(object.name));
        // the generated name of a field index is `<table>__<field>_index`: a typed index may not
        // take it (schema sync would report the clash, this does at registry build)
        if let Some(field) = fields.iter().find(|f| {
            f.index.is_some() && crate::ident::index_name(table, f.name) == object.full_name
        }) {
            return Err(Error::programming(format!(
                "{}: its database name {} is the name of the index of field '{}'; rename the index",
                at(),
                object.full_name,
                shown(field.name)
            )));
        }
        if index.fields.is_empty() {
            return Err(Error::programming(format!(
                "{} needs at least one field",
                at()
            )));
        }
        let columns = index.fields.len().saturating_add(index.include.len());
        if columns > MAX_INDEX_COLUMNS {
            return Err(Error::programming(format!(
                "{} has {columns} columns; PostgreSQL allows at most {MAX_INDEX_COLUMNS}",
                at()
            )));
        }
        if !index.include.is_empty() && !index.method.allows_include() {
            return Err(Error::programming(format!(
                "{}: include is only allowed with method \"btree\" or \"gist\" (this index uses \
                 \"{}\")",
                at(),
                index.method.as_str()
            )));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(columns);
        for (position, name) in index.fields.iter().chain(index.include).enumerate() {
            let key = position < index.fields.len();
            let shown_name = shown(name);
            let field = by_name
                .get(name)
                .map(|id| &fields[id.index()])
                .ok_or_else(|| {
                    Error::programming(format!("{}: unknown field '{shown_name}'", at()))
                })?;
            // `id` has no column of its own in the field list (the table is created with it),
            // but it is a column: a covering index may carry it (`INCLUDE (id)`)
            if !field.has_column() && field.ty != FieldType::Id {
                return Err(Error::programming(format!(
                    "{}: field '{shown_name}' is not stored in the model's table",
                    at()
                )));
            }
            if seen.contains(name) {
                return Err(Error::programming(if key || !index.fields.contains(name) {
                    format!("{}: field '{shown_name}' is listed twice", at())
                } else {
                    format!(
                        "{}: include field '{shown_name}' is already a key field",
                        at()
                    )
                }));
            }
            seen.push(name);
            if key && field.translate && index.method != IndexMethod::Trigram {
                return Err(Error::programming(format!(
                    "{}: field '{shown_name}' is translated; translated fields only support trigram indexes",
                    at()
                )));
            }
            if key && let Some(types) = index.method.field_types() {
                let (lookup, described) =
                    column_type_name(field.ty.type_name(), field.company_dependent);
                if !types.contains(&lookup) {
                    return Err(Error::programming(format!(
                        "{}: method = \"{}\" is for {} fields ('{shown_name}' is {described})",
                        at(),
                        index.method.as_str(),
                        and_list(types)
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_round_trip_and_properties() {
        let all = [
            IndexMethod::Btree,
            IndexMethod::Brin,
            IndexMethod::Gin,
            IndexMethod::Gist,
            IndexMethod::Trigram,
        ];
        for method in all {
            assert_eq!(IndexMethod::from_name(method.as_str()), Some(method));
        }
        let ams: Vec<_> = all.iter().map(|m| m.access_method()).collect();
        assert_eq!(ams, ["btree", "brin", "gin", "gist", "gin"]);
        let include: Vec<_> = all.iter().map(|m| m.allows_include()).collect();
        assert_eq!(include, [true, false, false, true, false]);
        assert_eq!(IndexMethod::from_name("btree_not_null"), None);
        assert_eq!(IndexMethod::from_name("Btree"), None);
    }

    #[test]
    fn and_lists() {
        assert_eq!(and_list(&[]), "");
        assert_eq!(and_list(&["json"]), "json");
        assert_eq!(and_list(&["a", "b"]), "a and b");
        assert_eq!(and_list(&["a", "b", "c"]), "a, b and c");
    }

    #[test]
    fn index_kind_mismatches() {
        assert_eq!(
            index_kind_mismatch(IndexKind::Gin, "date").as_deref(),
            Some("index = \"gin\" is for json fields (this field is date)")
        );
        for ok in ["date", "datetime", "integer"] {
            assert_eq!(index_kind_mismatch(IndexKind::Brin, ok), None, "{ok}");
        }
        assert_eq!(index_kind_mismatch(IndexKind::Btree, "json"), None);
        assert_eq!(index_kind_mismatch(IndexKind::Trigram, "char"), None);
    }
}
