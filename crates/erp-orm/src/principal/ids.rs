//! Identifier newtypes of the principal model (DN §2.1): users, groups, companies, API keys, links,
//! and the qualified XML ids an elevation scope lifts.

use std::fmt;
use std::num::NonZeroI64;

use super::error::{IdKind, PrincipalError};

/// ```
/// use erp_orm::{SUPERUSER_ID, UserId};
/// let uid = UserId::new(7)?;
/// assert_eq!(uid.get(), 7);
/// assert_eq!(SUPERUSER_ID, 1);
/// assert!(UserId::new(0).is_err());
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(NonZeroI64);

impl UserId {
    /// A user id.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidId`] for 0.
    pub fn new(uid: i64) -> Result<Self, PrincipalError> {
        Self::from_raw(uid).ok_or(PrincipalError::InvalidId {
            kind: IdKind::User,
            value: uid,
        })
    }

    /// The subject of the placeholder principal a credentialed transaction holds until its check
    /// accepted (`access::credential`): an id no `res.users` row has.
    pub(crate) const CREDENTIAL_PLACEHOLDER: Self = Self(NonZeroI64::MAX);

    /// `None` for 0 (the "keep this environment" value of `with_user`).
    pub(crate) const fn from_raw(uid: i64) -> Option<Self> {
        match NonZeroI64::new(uid) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The raw id.
    pub const fn get(self) -> i64 {
        self.0.get()
    }
}

impl PartialEq<i64> for UserId {
    fn eq(&self, other: &i64) -> bool {
        self.get() == *other
    }
}

impl PartialEq<UserId> for i64 {
    fn eq(&self, other: &UserId) -> bool {
        *self == other.get()
    }
}

impl From<UserId> for i64 {
    fn from(uid: UserId) -> Self {
        uid.get()
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Compares
/// with `i64` (`env.uid() == SUPERUSER_ID`).
#[allow(
    clippy::expect_used,
    reason = "evaluated at compile time: 1 is non-zero"
)]
pub const SUPERUSER_ID: UserId = UserId(NonZeroI64::new(1).expect("1 is non-zero"));

macro_rules! row_id {
    ($(#[$doc:meta])* $name:ident, $kind:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroI64);

        impl $name {
            /// The id of a database row.
            ///
            /// # Errors
            ///
            /// [`PrincipalError::InvalidId`] unless `id` is positive.
            pub fn new(id: i64) -> Result<Self, PrincipalError> {
                match NonZeroI64::new(id) {
                    Some(n) if id > 0 => Ok(Self(n)),
                    // zero or negative
                    _ => Err(PrincipalError::InvalidId {
                        kind: IdKind::$kind,
                        value: id,
                    }),
                }
            }

            /// The raw id.
            pub fn get(self) -> i64 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

row_id!(
    /// A `res.groups` id (positive).
    GroupId,
    Group
);
row_id!(
    /// A `res.company` id (positive).
    CompanyId,
    Company
);
row_id!(
    /// A `res.users.apikeys` id (positive); the key of an agent or service principal.
    ApiKeyId,
    ApiKey
);
row_id!(
    /// A link-token id (positive); the actor of a link principal.
    LinkId,
    Link
);

/// Longest qualified XML id accepted (bytes).
pub const MAX_XML_ID_LEN: usize = 256;

/// A fully qualified external id `module.name`, e.g. the record rule an elevation scope lifts
/// (`ScopeItem::lift`, DN §4).
///
/// Rules (the qualified form of `erp-data`'s `XmlId`): both parts non-empty, exactly one dot, no
/// whitespace or control characters, module part ASCII letters, digits and `_`, at most
/// [`MAX_XML_ID_LEN`] bytes.
///
/// ```
/// use erp_orm::QualifiedXmlId;
/// let id = QualifiedXmlId::parse("sale.sale_order_personal_rule")?;
/// assert_eq!((id.module(), id.name()), ("sale", "sale_order_personal_rule"));
/// assert!(QualifiedXmlId::parse("no_module").is_err());
/// # Ok::<(), erp_orm::PrincipalError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedXmlId {
    full: Box<str>,
    dot: usize,
}

impl QualifiedXmlId {
    /// Parse `module.name`.
    ///
    /// # Errors
    ///
    /// [`PrincipalError::InvalidXmlId`] naming the problem.
    pub fn parse(value: &str) -> Result<Self, PrincipalError> {
        let fail = |problem: &'static str| PrincipalError::InvalidXmlId {
            value: truncate(value),
            problem,
        };
        if value.len() > MAX_XML_ID_LEN {
            return Err(fail("is longer than 256 bytes"));
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(fail("contains whitespace or control characters"));
        }
        let Some((module, name)) = value.split_once('.') else {
            return Err(fail("has no module part"));
        };
        if module.is_empty() {
            return Err(fail("has an empty module part"));
        }
        if name.is_empty() {
            return Err(fail("has an empty name part"));
        }
        if name.contains('.') {
            return Err(fail("has more than one dot"));
        }
        if !module
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(fail(
                "has a module part that is not ASCII letters, digits and '_'",
            ));
        }
        Ok(Self {
            full: value.into(),
            dot: module.len(),
        })
    }

    /// The module part.
    pub fn module(&self) -> &str {
        &self.full[..self.dot]
    }

    /// The name part.
    pub fn name(&self) -> &str {
        &self.full[self.dot + 1..]
    }

    /// `module.name`.
    pub fn as_str(&self) -> &str {
        &self.full
    }
}

impl fmt::Display for QualifiedXmlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

/// At most 64 characters of an input echoed in an error message.
pub(crate) fn truncate(value: &str) -> String {
    const MAX: usize = 64;
    match value.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_refuses_zero_accepts_negative_and_compares_with_i64() {
        let err = UserId::new(0).unwrap_err();
        assert_eq!(err.to_string(), "invalid user id 0: ids are non-zero");
        assert_eq!(
            erp_fields::Error::from(err).kind(),
            erp_fields::ErrorKind::Programming
        );
        assert_eq!(UserId::new(-3).unwrap().get(), -3);
        assert_eq!(UserId::new(i64::MIN).unwrap().get(), i64::MIN);
        assert!(UserId::new(5).unwrap() == 5);
        assert!(5 == UserId::new(5).unwrap());
        assert_eq!(i64::from(UserId::new(9).unwrap()), 9);
    }

    #[test]
    fn row_ids_are_positive() {
        for bad in [0, -1, i64::MIN] {
            let err = GroupId::new(bad).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("invalid group id {bad}: ids are positive")
            );
        }
        assert_eq!(
            CompanyId::new(0).unwrap_err().to_string(),
            "invalid company id 0: ids are positive"
        );
        assert_eq!(
            ApiKeyId::new(-2).unwrap_err().to_string(),
            "invalid API key id -2: ids are positive"
        );
        assert_eq!(
            LinkId::new(0).unwrap_err().to_string(),
            "invalid link id 0: ids are positive"
        );
        assert_eq!(CompanyId::new(i64::MAX).unwrap().get(), i64::MAX);
        assert_eq!(GroupId::new(1).unwrap().get(), 1);
    }

    #[test]
    fn qualified_xml_ids() {
        let ok = QualifiedXmlId::parse("base.rule_ü").unwrap();
        assert_eq!(ok.module(), "base");
        assert_eq!(ok.name(), "rule_ü");
        assert_eq!(ok.to_string(), "base.rule_ü");
        let cases = [
            ("", "has no module part"),
            ("plain", "has no module part"),
            (".x", "has an empty module part"),
            ("m.", "has an empty name part"),
            ("a.b.c", "has more than one dot"),
            (
                "mö.x",
                "has a module part that is not ASCII letters, digits and '_'",
            ),
            ("m.a b", "contains whitespace or control characters"),
            ("m.a\r\nb", "contains whitespace or control characters"),
        ];
        for (input, problem) in cases {
            let err = QualifiedXmlId::parse(input).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("invalid XML id '{input}': it {problem}"),
                "{input:?}"
            );
        }
        let long = format!("m.{}", "x".repeat(MAX_XML_ID_LEN));
        let err = QualifiedXmlId::parse(&long).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "invalid XML id '{}…': it is longer than 256 bytes",
                &long[..64]
            )
        );
    }
}
