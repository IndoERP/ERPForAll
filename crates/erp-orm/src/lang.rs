//! Language validation and active language discovery for translated fields.

use erp_fields::{Error, Result};
use rustc_hash::FxHashSet;

/// Active language state for a transaction.
#[derive(Debug, Clone)]
pub struct ActiveLanguages {
    active_codes: FxHashSet<String>,
}

impl ActiveLanguages {
    /// The state for exactly these active codes.
    pub fn new(active_codes: FxHashSet<String>) -> Self {
        Self { active_codes }
    }

    /// Language codes currently active in `res.lang`.
    pub fn codes(&self) -> &FxHashSet<String> {
        &self.active_codes
    }

    /// Whether `en_US` is currently active.
    pub fn english_active(&self) -> bool {
        self.active_codes.contains("en_US")
    }
}

/// Validate that `lang` is valid to read or write translated fields.
///
/// Unset/empty language or `"en_US"` is always accepted. Other codes must be present in
/// `active_codes` (which reflects active records in `res.lang`).
pub fn validate_lang(lang: Option<&str>, active_codes: &FxHashSet<String>) -> Result<()> {
    match lang {
        None | Some("") | Some("en_US") => Ok(()),
        Some(code) => {
            if code.len() > 64
                || !code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_@.-".contains(&b))
                || !active_codes.contains(code)
            {
                Err(Error::user(format!("Invalid language code: {code}")))
            } else {
                Ok(())
            }
        }
    }
}

impl crate::Transaction {
    /// Resolve non-default language membership before any query shape or order memo is built.
    pub(crate) async fn validate_search_language(&mut self, env: &crate::Env) -> Result<()> {
        let Some(code) = env.lang().filter(|s| !s.is_empty() && *s != "en_US") else {
            return Ok(());
        };
        // Reject malformed/oversized input before querying or copying it into a memo.
        if code.len() > 64
            || !code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_@.-".contains(&b))
        {
            return Err(Error::user(format!("Invalid language code: {code}")));
        }
        let active = self.active_languages().await?;
        validate_lang(env.lang(), active.codes())
    }

    /// Synchronous compilers may only use a non-default language already admitted asynchronously.
    pub(crate) fn validate_cached_search_language(&self, env: &crate::Env) -> Result<()> {
        if env.lang().is_none_or(|s| s.is_empty() || s == "en_US") {
            return Ok(());
        }
        match &self.active_langs {
            Some(active) => validate_lang(env.lang(), active.codes()),
            None => Err(Error::programming(
                "search language used before active languages were loaded",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_lang() {
        let mut active = FxHashSet::default();
        active.insert("en_US".to_string());
        active.insert("fr_FR".to_string());

        assert!(validate_lang(None, &active).is_ok());
        assert!(validate_lang(Some(""), &active).is_ok());
        assert!(validate_lang(Some("en_US"), &active).is_ok());
        assert!(validate_lang(Some("fr_FR"), &active).is_ok());

        let err = validate_lang(Some("nl_NL"), &active).unwrap_err();
        assert_eq!(err.message(), "Invalid language code: nl_NL");

        let err = validate_lang(Some("Dummy"), &active).unwrap_err();
        assert_eq!(err.message(), "Invalid language code: Dummy");

        let err = validate_lang(Some("'', NOW("), &active).unwrap_err();
        assert_eq!(err.message(), "Invalid language code: '', NOW(");

        let long_code = "a".repeat(65);
        let err = validate_lang(Some(&long_code), &active).unwrap_err();
        assert!(err.message().starts_with("Invalid language code: "));
    }
}
