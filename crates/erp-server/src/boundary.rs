//! Strict, secret-redacted HTTP boundary values (0037, 0061).
use thiserror::Error;

/// A boundary refusal; no rejected credential or URL is retained in diagnostics.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum BoundaryError {
    /// A presented Authorization header was ambiguous, malformed or oversized.
    #[error("Invalid apikey")]
    Authorization,
    /// Redirect was not an allowed local shell URL.
    #[error("redirect must be an allowed local shell URL")]
    Redirect,
    /// Cookie transport was ambiguous or malformed.
    #[error("ambiguous or malformed authentication cookie")]
    Cookie,
    /// Cookie/header database disagreement.
    #[error("Session cookie and X-ERP-Database header refer to different databases.")]
    DatabaseMismatch,
    /// The requested database is unavailable.
    #[error("Database not found.")]
    Database,
    /// Browser request was not same-origin.
    #[error("Missing \"Authorization\" or Sec-headers for interactive usage.")]
    SameOrigin,
}

impl BoundaryError {
    /// The variant's fixed text (the same as its `Display`), for a log field. It never carries
    /// the refused value, so a URL, cookie or credential cannot reach the log through it.
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::Authorization => "Invalid apikey",
            Self::Redirect => "redirect must be an allowed local shell URL",
            Self::Cookie => "ambiguous or malformed authentication cookie",
            Self::DatabaseMismatch => {
                "Session cookie and X-ERP-Database header refer to different databases."
            }
            Self::Database => "Database not found.",
            Self::SameOrigin => "Missing \"Authorization\" or Sec-headers for interactive usage.",
        }
    }
}

/// Parse exactly one bounded Bearer header, retaining no submitted text in errors.
/// Token grammar and cryptographic verification belong to `erp-security`; malformed tokens
/// still reach its address throttle. A missing header permits the browser session path.
/// # Errors
/// Duplicate headers, another scheme, whitespace in the token, non-ASCII or oversized input.
pub fn bearer<'a>(headers: &[&'a str]) -> Result<Option<&'a str>, BoundaryError> {
    let value = match headers {
        [] => return Ok(None),
        [value] => *value,
        _ => return Err(BoundaryError::Authorization),
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return Err(BoundaryError::Authorization);
    };
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > 1024
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(BoundaryError::Authorization);
    }
    Ok(Some(token))
}

/// Read exactly one named authentication cookie.
/// # Errors
/// Refuses duplicate/malformed values rather than choosing one.
pub fn cookie<'a>(headers: &'a [&str], name: &str) -> Result<Option<&'a str>, BoundaryError> {
    let mut found = None;
    for header in headers {
        if header.len() > 8192 || header.bytes().any(|b| b.is_ascii_control()) {
            return Err(BoundaryError::Cookie);
        }
        for pair in header.split(';') {
            let pair = pair.trim();
            let Some((key, value)) = pair.split_once('=') else {
                if pair == name {
                    return Err(BoundaryError::Cookie);
                }
                continue; // Unrelated cookies belong to other applications, not our auth input.
            };
            if key == name {
                if found.is_some()
                    || value.is_empty()
                    || value.len() > 1024
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                {
                    return Err(BoundaryError::Cookie);
                }
                found = Some(value);
            }
        }
    }
    Ok(found)
}

/// Select the explicitly configured database, checking header/cookie agreement.
/// # Errors
/// Refuses ambiguity and unknown databases.
pub fn database<'a>(
    configured: &'a str,
    header: Option<&str>,
    cookie: Option<&str>,
) -> Result<&'a str, BoundaryError> {
    if let (Some(header), Some(cookie)) = (header, cookie)
        && header != cookie
    {
        return Err(BoundaryError::DatabaseMismatch);
    }
    if header.is_some_and(|name| name != configured)
        || cookie.is_some_and(|name| name != configured)
    {
        return Err(BoundaryError::Database);
    }
    Ok(configured)
}

/// Cookie-authenticated unsafe JSON must come from a same-origin browser fetch.
/// # Errors
/// Missing or cross-site metadata is refused. Origin may be absent for a browser
/// same-origin request; when supplied it must equal the configured canonical origin.
pub fn same_origin_fetch(
    site: Option<&str>,
    origin: Option<&str>,
    canonical: &str,
) -> Result<(), BoundaryError> {
    if !matches!(site, Some("same-origin" | "none"))
        || origin.is_some_and(|origin| origin != canonical)
    {
        return Err(BoundaryError::SameOrigin);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log reason of every variant is its displayed text, so the two never drift apart.
    #[test]
    fn each_reason_is_the_displayed_text() {
        for error in [
            BoundaryError::Authorization,
            BoundaryError::Redirect,
            BoundaryError::Cookie,
            BoundaryError::DatabaseMismatch,
            BoundaryError::Database,
            BoundaryError::SameOrigin,
        ] {
            assert_eq!(error.reason(), error.to_string(), "{error:?}");
        }
    }
}
