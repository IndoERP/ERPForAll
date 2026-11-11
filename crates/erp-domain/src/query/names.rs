//! Validated names: models, fields and group-by paths. Whether a name exists is checked against
//! the registry when a query runs; here only its form is.

use std::fmt;

use crate::param::MAX_PATH_FIELD_LEN;

use super::error::{Bound, JsonPath, QueryAstError, QueryAstReason, shown};

/// Longest model name, as in the JSON-2 routes of `erp-rpc`.
pub(crate) const MAX_MODEL_NAME_LEN: usize = 128;

fn invalid(at: &JsonPath, what: &'static str, value: &str, why: &'static str) -> QueryAstError {
    QueryAstError::new(
        at,
        QueryAstReason::InvalidName {
            what,
            value: shown(value),
            why,
        },
    )
}

/// A model name: dot-separated lowercase identifiers (`res.partner`), at most 128 bytes.
///
/// ```
/// use erp_domain::query::ModelName;
///
/// assert_eq!(ModelName::new("account.move")?.as_str(), "account.move");
/// assert!(ModelName::new("Account.Move").is_err());
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelName(Box<str>);

impl ModelName {
    /// Check `name`.
    ///
    /// # Errors
    ///
    /// [`QueryAstReason::InvalidName`] (at `$`) when the name is empty, longer than 128 bytes, has
    /// an empty dot-separated part, or holds anything but `a-z`, `0-9`, `_` and `.`.
    pub fn new(name: &str) -> Result<Self, QueryAstError> {
        Self::at(&JsonPath::root(), name)
    }

    pub(crate) fn at(at: &JsonPath, name: &str) -> Result<Self, QueryAstError> {
        const WHAT: &str = "model name";
        if name.is_empty() {
            return Err(invalid(at, WHAT, name, "it is empty"));
        }
        if name.len() > MAX_MODEL_NAME_LEN {
            return Err(invalid(at, WHAT, name, "it is longer than 128 bytes"));
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        {
            return Err(invalid(
                at,
                WHAT,
                name,
                "only a-z, 0-9, '_' and '.' are allowed",
            ));
        }
        if name.split('.').any(str::is_empty) {
            return Err(invalid(at, WHAT, name, "it has an empty part"));
        }
        Ok(Self(Box::from(name)))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A field name: `[a-z_][a-z0-9_]*`, at most 63 bytes, not starting with `__` (those are
/// reserved for spec names such as `__count`).
///
/// ```
/// use erp_domain::query::FieldName;
///
/// assert_eq!(FieldName::new("partner_id")?.as_str(), "partner_id");
/// assert!(FieldName::new("__count").is_err());
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldName(Box<str>);

impl FieldName {
    /// Check `name`.
    ///
    /// # Errors
    ///
    /// [`QueryAstReason::InvalidName`] (at `$`) when the name is not of the form above.
    pub fn new(name: &str) -> Result<Self, QueryAstError> {
        Self::at(&JsonPath::root(), name)
    }

    pub(crate) fn at(at: &JsonPath, name: &str) -> Result<Self, QueryAstError> {
        const WHAT: &str = "field name";
        let mut bytes = name.bytes();
        if name.is_empty() {
            return Err(invalid(at, WHAT, name, "it is empty"));
        }
        if name.len() > MAX_PATH_FIELD_LEN {
            return Err(invalid(at, WHAT, name, "it is longer than 63 bytes"));
        }
        if !bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            || !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(invalid(at, WHAT, name, "it must match [a-z_][a-z0-9_]*"));
        }
        if name.starts_with("__") {
            return Err(invalid(
                at,
                WHAT,
                name,
                "names starting with '__' are reserved",
            ));
        }
        Ok(Self(Box::from(name)))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A group-by path: field names joined by dots, following many2one fields
/// (`partner_id.country_id`), at most 4 segments by default.
///
/// ```
/// use erp_domain::query::FieldPath;
///
/// let path = FieldPath::parse("partner_id.country_id")?;
/// assert_eq!(path.segments().len(), 2);
/// assert_eq!(path.to_string(), "partner_id.country_id");
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldPath(Box<[FieldName]>);

impl FieldPath {
    /// Check `dotted` against the default segment bound.
    ///
    /// # Errors
    ///
    /// [`QueryAstReason::InvalidName`] for an invalid segment, [`QueryAstReason::TooLarge`] for
    /// more than 4 segments.
    pub fn parse(dotted: &str) -> Result<Self, QueryAstError> {
        Self::at(
            &JsonPath::root(),
            dotted,
            super::limits::DEFAULT_PATH_SEGMENTS,
        )
    }

    pub(crate) fn at(
        at: &JsonPath,
        dotted: &str,
        max_segments: usize,
    ) -> Result<Self, QueryAstError> {
        let mut segments = Vec::new();
        for part in dotted.split('.') {
            // the bound is checked before the next segment is kept: a long path is not built
            if segments.len() == max_segments {
                return Err(QueryAstError::new(
                    at,
                    QueryAstReason::TooLarge {
                        bound: Bound::PathSegments,
                        max: super::limits::Widen::widened(max_segments),
                        found: dotted.split('.').count().to_string(),
                    },
                ));
            }
            segments.push(FieldName::at(at, part)?);
        }
        Ok(Self(segments.into()))
    }

    /// The segments, in order (at least one).
    pub fn segments(&self) -> &[FieldName] {
        &self.0
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(segment.as_str())?;
        }
        Ok(())
    }
}
