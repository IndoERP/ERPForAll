//! The session token field plan (`res.users._get_session_token_fields`, H58 TOKEN; design:
//! ).
//!
//! A session's token covers the user fields `id`, `login`, `password` and `active`; a change of
//! any of them ends the user's sessions at their next check. An addon that adds a credential
//! factor (a TOTP secret, a linked account's token) overrides the dispatched method
//! `_get_session_token_fields` and adds the **names** of the user fields whose change must end
//! sessions too, after `super`:
//!
//! ```ignore
//! #[method(override)]
//! async fn _get_session_token_fields(
//!     this: &RecordSet<ResUsers>,
//!     tx: &mut Transaction,
//!     sup: Super,
//! ) -> Result<SessionTokenFields> {
//!     let fields: SessionTokenFields = sup.call(tx, this.as_records(), ()).await?;
//!     fields.with_field("totp_secret")
//! }
//! ```
//!
//! The result is a bounded set of names, resolved **once** when the registry's authentication
//! service is built and checked against `res.users` there (a name that is not a stored scalar
//! column of the user refuses the whole plan, loudly); the framework reads the values in the
//! same statements that read the base fields, and encodes them. The hook returns no SQL, no
//! value and no per-user decision, and cannot remove a base field.

use std::collections::BTreeSet;

use erp_fields::{Error, FieldType, Result};
use erp_registry::Registry;
use tokio_postgres::Row;
use zeroize::Zeroizing;

use super::session::{ExtraKind, ExtraValue};

/// The dispatched method this module serves.
pub(crate) const METHOD: &str = "_get_session_token_fields";

/// The fields every token covers; no override can remove one.
pub const BASE_FIELDS: [&str; 4] = ["id", "login", "password", "active"];

/// Most fields an addon chain may add to the base set.
pub const MAX_EXTRA_FIELDS: usize = 16;

/// Largest `size` of a char field a token may cover: its value stays within the 64 KiB value
/// limit even at four bytes per character.
pub const MAX_CHAR_SIZE: u32 = 16_384;

/// Longest field name, bytes (a PostgreSQL identifier).
pub const MAX_FIELD_NAME_BYTES: usize = 63;

/// The user fields a session token covers (the result of `_get_session_token_fields`).
///
/// Starts from [`SessionTokenFields::base`] and only grows: see
/// [`SessionTokenFields::with_field`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenFields {
    extra: BTreeSet<String>,
}

impl SessionTokenFields {
    /// The base fields alone (`id`, `login`, `password`, `active`).
    pub const fn base() -> Self {
        Self {
            extra: BTreeSet::new(),
        }
    }

    /// Also cover the field `name` of `res.users`.
    ///
    /// The name is checked against the registry when the authentication service is built: it
    /// must be a stored, non-translated column of type boolean, integer, selection, many2one, or
    /// char with a size of at most [`MAX_CHAR_SIZE`]. Float, text, unsized char, date and datetime
    /// fields are refused (their text form depends on session settings, or their size is not
    /// bounded). Relation contributions (`one2many`, `many2many`) are not supported
    /// yet and refuse the plan.
    ///
    /// ```
    /// use erp_security::auth::SessionTokenFields;
    ///
    /// # fn main() -> Result<(), erp_fields::Error> {
    /// let fields = SessionTokenFields::base()
    ///     .with_field("totp_secret")?
    ///     .with_field("oauth_access_token")?;
    /// // in name order, the order the token encodes them
    /// assert_eq!(
    ///     fields.extra_fields().collect::<Vec<_>>(),
    ///     ["oauth_access_token", "totp_secret"]
    /// );
    /// // a base field cannot be added again, nor a name twice
    /// assert!(fields.clone().with_field("login").is_err());
    /// assert!(fields.with_field("totp_secret").is_err());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// `Validation` when `name` is empty, longer than [`MAX_FIELD_NAME_BYTES`], not of the form
    /// `[a-z_][a-z0-9_]*`, already in the set (a base field, or added before: a duplicate is
    /// refused, not merged), or when the set would pass [`MAX_EXTRA_FIELDS`] added fields.
    pub fn with_field(mut self, name: &str) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::validation(format!(
                "_get_session_token_fields: '{}' is not a field name (lowercase letters, digits \
                 and underscores, at most {MAX_FIELD_NAME_BYTES} bytes, not starting with a digit)",
                shown(name)
            )));
        }
        if BASE_FIELDS.contains(&name) {
            return Err(Error::validation(format!(
                "_get_session_token_fields: '{name}' is already part of the base token"
            )));
        }
        if self.extra.contains(name) {
            return Err(Error::validation(format!(
                "_get_session_token_fields: '{name}' was added twice"
            )));
        }
        if self.extra.len() >= MAX_EXTRA_FIELDS {
            return Err(Error::validation(format!(
                "_get_session_token_fields: more than {MAX_EXTRA_FIELDS} added fields"
            )));
        }
        self.extra.insert(name.to_owned());
        Ok(self)
    }

    /// The added field names, in name order (the order the token encodes them).
    pub fn extra_fields(&self) -> impl Iterator<Item = &str> {
        self.extra.iter().map(String::as_str)
    }

    /// Whether the plan is the base fields alone.
    pub fn is_base(&self) -> bool {
        self.extra.is_empty()
    }
}

impl Default for SessionTokenFields {
    fn default() -> Self {
        Self::base()
    }
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    bytes.len() <= MAX_FIELD_NAME_BYTES
        && (first.is_ascii_lowercase() || *first == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// `name` cut for an error text (a hook can pass any string).
fn shown(name: &str) -> String {
    name.chars().take(40).collect()
}

/// The plan of one registry: the added fields checked against `res.users`, in name order, with
/// the statements' fragments they need. Immutable; rebuilt with the registry, never per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenPlan {
    fields: Vec<PlanField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanField {
    name: String,
    kind: ExtraKind,
}

/// `sql` with `text` in place of `marker`, which must occur exactly once: a statement that lost
/// or repeated a marker would silently read or guard the wrong fields.
///
/// # Errors
///
/// `Programming` naming the marker and how often it occurs.
pub(crate) fn replace_once(sql: &str, marker: &str, text: &str) -> Result<String> {
    let found = sql.matches(marker).count();
    if found != 1 {
        return Err(Error::programming(format!(
            "the session token marker {marker} occurs {found} times in a statement, not once"
        )));
    }
    Ok(sql.replacen(marker, text, 1))
}

/// A marker inside a statement where [`TokenPlan::select_list`] goes (a harmless SQL comment while
/// the plan is the base one).
pub(crate) const COLUMNS_MARKER: &str = "/*token_cols*/";

/// A marker inside a statement where [`TokenPlan::guard`] goes.
pub(crate) const GUARD_MARKER: &str = "/*token_guard*/";

impl TokenPlan {
    /// The base fields alone: no statement changes.
    /// A plan of `names` as text fields, for unit tests of the statements (no registry).
    #[cfg(test)]
    pub(crate) fn text_fields_for_tests(names: &[&str]) -> Self {
        Self {
            fields: names
                .iter()
                .map(|n| PlanField {
                    name: (*n).to_owned(),
                    kind: ExtraKind::Text,
                })
                .collect(),
        }
    }

    pub(crate) const fn base() -> Self {
        Self { fields: Vec::new() }
    }

    pub(crate) fn is_base(&self) -> bool {
        self.fields.is_empty()
    }

    /// How many fields the plan adds to the base ones.
    pub(crate) fn len(&self) -> usize {
        self.fields.len()
    }

    /// The plan `fields` makes in `registry`.
    ///
    /// # Errors
    ///
    /// `Programming` naming the first added field `res.users` does not have, does not store as a
    /// plain column, or stores as a type or relation a token does not cover.
    pub(crate) fn compile(fields: &SessionTokenFields, registry: &Registry) -> Result<Self> {
        let Some(users) = registry.model("res.users") else {
            return Err(Error::programming(
                "_get_session_token_fields: the registry has no res.users",
            ));
        };
        let mut compiled = Vec::with_capacity(fields.extra.len());
        for name in fields.extra_fields() {
            let refuse = |why: String| {
                Err(Error::programming(format!(
                    "{METHOD}: res.users.{name} {why}"
                )))
            };
            let Some(spec) = users.field(name) else {
                return Err(Error::programming(format!(
                    "{METHOD}: res.users has no field '{name}'"
                )));
            };
            if matches!(
                spec.ty,
                FieldType::One2many { .. } | FieldType::Many2many { .. }
            ) {
                return refuse(
                    "is a relation field, relation contributions are not supported until \
                     a later release"
                        .to_owned(),
                );
            }
            if !spec.store || !spec.has_column() {
                return refuse(
                    "is not stored, only stored columns can be part of the token".into(),
                );
            }
            if spec.translate || spec.company_dependent {
                return refuse(
                    "is translated or company dependent, which a token cannot cover".into(),
                );
            }
            let kind = match spec.ty {
                FieldType::Boolean => ExtraKind::Bool,
                FieldType::Integer => ExtraKind::Int,
                FieldType::Char { size: Some(size) } if size <= MAX_CHAR_SIZE => ExtraKind::Text,
                FieldType::Char { size } => {
                    return refuse(format!(
                        "is a char with {}, so its value is not bounded (a token covers a char of \
                         size 1 to {MAX_CHAR_SIZE})",
                        size.map_or("no size".to_owned(), |s| format!("size {s}"))
                    ));
                }
                FieldType::Text => {
                    return refuse("is a text field, so its value is not bounded".into());
                }
                FieldType::Selection { .. } => ExtraKind::Text,
                FieldType::Many2one { .. } => ExtraKind::Id,
                ref other => {
                    return refuse(format!(
                        "has a type a token cannot cover ({})",
                        other.type_name()
                    ));
                }
            };
            compiled.push(PlanField {
                name: name.to_owned(),
                kind,
            });
        }
        Ok(Self { fields: compiled })
    }

    /// The columns of the added fields over the alias `alias` (none when it is empty), each as
    /// text and with a leading comma, for the end of a select list; empty for the base plan.
    /// Names are checked identifiers of the registry, quoted here.
    pub(crate) fn select_list(&self, alias: &str) -> String {
        let dot = if alias.is_empty() { "" } else { "." };
        self.fields
            .iter()
            .map(|f| format!(", {alias}{dot}\"{}\"::text", f.name))
            .collect()
    }

    /// One `NULL::text` with a leading comma per added field (the padding of a `UNION` arm that
    /// has no user row).
    pub(crate) fn null_list(&self) -> String {
        ", NULL::text".repeat(self.fields.len())
    }

    /// A condition (with a leading `AND`) that the added fields of the row `alias` still equal the
    /// array bound at parameter `$param` (the values phase A read); empty for the base plan.
    pub(crate) fn guard(&self, alias: &str, param: usize) -> String {
        if self.is_base() {
            return String::new();
        }
        let columns: Vec<String> = self
            .fields
            .iter()
            .map(|f| format!("{alias}.\"{}\"::text", f.name))
            .collect();
        format!(
            "AND ARRAY[{}]::text[] IS NOT DISTINCT FROM ${param}::text[]",
            columns.join(", ")
        )
    }

    /// The values of the added fields from `row`, whose first one is column `first`.
    ///
    /// # Errors
    ///
    /// `Database` when a column cannot be decoded as text.
    pub(crate) fn read_values(&self, row: &Row, first: usize) -> Result<TokenValues> {
        let mut values = Vec::with_capacity(self.fields.len());
        for (offset, field) in self.fields.iter().enumerate() {
            let value: Option<String> = row.try_get(first + offset).map_err(|e| {
                Error::db(format!(
                    "could not decode the session token field '{}': {e}",
                    field.name
                ))
                .with_source(e)
            })?;
            values.push(value.map(Zeroizing::new));
        }
        Ok(TokenValues(values))
    }

    /// `values` with their names and types, in name order, for the token encoder.
    ///
    /// # Errors
    ///
    /// `Programming` when `values` were not read by this plan (a different number of values).
    pub(crate) fn bind<'a>(&'a self, values: &'a TokenValues) -> Result<Vec<ExtraValue<'a>>> {
        if values.0.len() != self.fields.len() {
            return Err(Error::programming(
                "the session token values do not match the plan",
            ));
        }
        Ok(self
            .fields
            .iter()
            .zip(&values.0)
            .map(|(field, value)| ExtraValue {
                name: &field.name,
                kind: field.kind,
                value: value.as_deref().map(String::as_str),
            })
            .collect())
    }
}

/// The current values of a plan's added fields for one user, wiped on drop (a factor may be a
/// secret).
#[derive(Default)]
pub(crate) struct TokenValues(Vec<Option<Zeroizing<String>>>);

impl TokenValues {
    /// No values: the base plan's.
    pub(crate) const fn empty() -> Self {
        Self(Vec::new())
    }

    /// The values as the array parameter the success guard compares.
    pub(crate) fn as_array(&self) -> Vec<Option<&str>> {
        self.0
            .iter()
            .map(|v| v.as_deref().map(String::as_str))
            .collect()
    }
}

impl std::fmt::Debug for TokenValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenValues")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "unit tests")]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;

    #[test]
    fn the_base_plan_adds_nothing() {
        let plan = SessionTokenFields::base();
        assert!(plan.is_base());
        assert_eq!(plan.extra_fields().count(), 0);
        assert_eq!(plan, SessionTokenFields::default());
    }

    #[test]
    fn fields_are_kept_in_name_order() {
        let plan = SessionTokenFields::base()
            .with_field("zeta")
            .and_then(|p| p.with_field("alpha"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!plan.is_base());
        assert_eq!(
            plan.extra_fields().collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn invalid_duplicate_and_base_names_are_refused_with_their_text() {
        let long = "a".repeat(MAX_FIELD_NAME_BYTES + 1);
        for name in ["", "Bad", "1st", "a b", "a;b", "a\"b", "é", long.as_str()] {
            let error = SessionTokenFields::base().with_field(name).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{name:?}");
            assert!(
                error.message().contains("is not a field name"),
                "{name:?}: {error}"
            );
        }
        for name in BASE_FIELDS {
            let error = SessionTokenFields::base().with_field(name).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!("_get_session_token_fields: '{name}' is already part of the base token")
            );
        }
        let error = SessionTokenFields::base()
            .with_field("x")
            .and_then(|p| p.with_field("x"))
            .unwrap_err();
        assert_eq!(
            error.message(),
            "_get_session_token_fields: 'x' was added twice"
        );
    }

    #[test]
    fn a_marker_must_occur_exactly_once() {
        assert_eq!(replace_once("a /*m*/ b", "/*m*/", "X").unwrap(), "a X b");
        for sql in ["a b", "/*m*/ /*m*/"] {
            let error = replace_once(sql, "/*m*/", "X").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert!(error.message().contains("/*m*/"), "{error}");
        }
    }

    #[test]
    fn a_duplicate_is_reported_before_the_cap() {
        let mut plan = SessionTokenFields::base();
        for n in 0..MAX_EXTRA_FIELDS {
            plan = plan.with_field(&format!("f{n}")).unwrap();
        }
        let error = plan.with_field("f3").unwrap_err();
        assert_eq!(
            error.message(),
            "_get_session_token_fields: 'f3' was added twice"
        );
    }

    #[test]
    fn the_added_fields_are_bounded() {
        let mut plan = SessionTokenFields::base();
        for n in 0..MAX_EXTRA_FIELDS {
            plan = plan
                .with_field(&format!("f{n}"))
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let error = plan.with_field("one_more").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "_get_session_token_fields: more than 16 added fields"
        );
    }
}
