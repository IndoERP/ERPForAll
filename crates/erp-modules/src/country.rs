//! ISO 3166-1 alpha-2 country codes, as manifests and company records name them.

use crate::error::{Error, Result};

/// A two-letter country code, stored upper case (`be` and `BE` are the same code).
///
/// ```
/// use erp_modules::CountryCode;
/// let code = CountryCode::parse("be").unwrap();
/// assert_eq!(code.as_str(), "BE");
/// assert_eq!(code, CountryCode::parse("BE").unwrap());
/// assert!(CountryCode::parse("BEL").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct CountryCode(String);

impl CountryCode {
    /// Parse a code: exactly two ASCII letters, any case.
    ///
    /// # Errors
    /// [`Error::InvalidCountryCode`] for anything else (empty, one letter, digits, non-ASCII,
    /// surrounding whitespace, longer codes).
    pub fn parse(code: &str) -> Result<Self> {
        if code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()) {
            Ok(Self(code.to_ascii_uppercase()))
        } else {
            Err(Error::InvalidCountryCode {
                code: code.to_owned(),
            })
        }
    }

    /// The upper-case code.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CountryCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CountryCode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}
