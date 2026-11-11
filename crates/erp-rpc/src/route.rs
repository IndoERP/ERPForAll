//! Routes (design note §4 step 1, §6.1): `/erp/json/2/<model>/<method>` and `call_kw`'s
//! `(model, method)`. The path's model and method always win over the body.

use std::fmt;

use crate::error::{ProtocolError, RpcFailure};

/// Longest model or method name accepted before any lookup.
pub const MAX_NAME_LEN: usize = 128;

const DID_YOU_MEAN: &str = "Did you mean POST /erp/json/2/<model>/<method>?";

/// Which entry point a call came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RouteKind {
    /// `POST /erp/json/2/<model>/<method>`.
    Json2,
    /// `POST /erp/dataset/call_kw[/<path>]` (model and method from the JSON-RPC params).
    CallKw,
}

/// A validated route: a model and a method name made of `[a-z0-9_.]` (upper case allowed in the
/// method, as Python identifiers), at most [`MAX_NAME_LEN`] bytes each.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Route {
    kind: RouteKind,
    model: Box<str>,
    method: Box<str>,
}

impl Route {
    /// Parse a native or legacy JSON-2 path. Invalid model/method names answer a 404
    /// with the canonical route hint "Did you mean POST /erp/json/2/<model>/<method>?"; an invalid
    /// name is the same 404 as an unknown one would be after lookup (no name is echoed).
    ///
    /// ```
    /// use erp_rpc::Route;
    /// let route = Route::parse_json2("/json/2/res.partner/search_read").unwrap();
    /// assert_eq!((route.model(), route.method()), ("res.partner", "search_read"));
    /// assert_eq!(
    ///     Route::parse_json2("/json/2/res.partner").unwrap_err().to_string(),
    ///     "werkzeug.exceptions.NotFound (404): Did you mean POST /erp/json/2/<model>/<method>?"
    /// );
    /// ```
    ///
    /// # Errors
    ///
    /// [`ProtocolError::NotFound`] as above.
    pub fn parse_json2(path: &str) -> Result<Self, RpcFailure> {
        let not_found = || RpcFailure::Protocol(ProtocolError::NotFound(DID_YOU_MEAN.to_owned()));
        let rest = path
            .strip_prefix("/erp/json/2/")
            .or_else(|| path.strip_prefix("/json/2/"))
            .ok_or_else(not_found)?;
        let mut parts = rest.split('/');
        let (Some(model), Some(method), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(not_found());
        };
        if !valid_model(model) || !valid_method(method) {
            return Err(not_found());
        }
        Ok(Self {
            kind: RouteKind::Json2,
            model: model.into(),
            method: method.into(),
        })
    }

    /// A `call_kw` route (the transport parsed the JSON-RPC envelope). Names that are not
    /// well-formed get the 404 of an unknown model (`call_kw` has no "did you mean" route).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::NotFound`] "the model '<model>' does not exist" for a malformed model
    /// name; "The method '<model>.<method>' does not exist" for a malformed method name.
    pub fn call_kw(model: &str, method: &str) -> Result<Self, RpcFailure> {
        if !valid_model(model) {
            return Err(RpcFailure::Protocol(ProtocolError::NotFound(format!(
                "the model {} does not exist",
                py_repr(model)
            ))));
        }
        if !valid_method(method) {
            return Err(RpcFailure::Protocol(ProtocolError::NotFound(format!(
                "The method '{model}.{}' does not exist",
                truncate(method)
            ))));
        }
        Ok(Self {
            kind: RouteKind::CallKw,
            model: model.into(),
            method: method.into(),
        })
    }

    /// The entry point.
    pub fn kind(&self) -> RouteKind {
        self.kind
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The method name.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// `<model>.<method>`.
    pub fn entry(&self) -> String {
        format!("{}.{}", self.model, self.method)
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.model, self.method)
    }
}

fn valid_model(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

fn valid_method(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Python's `repr` of a well-formed name (single quotes; names never contain quotes).
pub(crate) fn py_repr(name: &str) -> String {
    format!("'{}'", truncate(name))
}

fn truncate(name: &str) -> &str {
    match name.char_indices().nth(MAX_NAME_LEN) {
        Some((i, _)) => &name[..i],
        None => name,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn erp_json2_routes_bind_the_same_entry_as_legacy_input() {
        let canonical = Route::parse_json2("/erp/json/2/res.partner/read").unwrap();
        let legacy = Route::parse_json2("/json/2/res.partner/read").unwrap();
        assert_eq!(canonical.entry(), legacy.entry());
        assert_eq!(canonical.kind(), legacy.kind());
        assert!(Route::parse_json2("/erp/json/2/res.partner/read/extra").is_err());
    }

    #[test]
    fn json2_paths() {
        let ok = Route::parse_json2("/json/2/test_http.stargate/search").unwrap();
        assert_eq!(ok.entry(), "test_http.stargate.search");
        assert_eq!(ok.kind(), RouteKind::Json2);
        for bad in [
            "/json/2",
            "/json/2/",
            "/json/2/res.users",
            "/json/2/res.users/",
            "/json/2//search",
            "/json/2/res.users/search/extra",
            "/json/2/Res.Users/search",
            "/json/2/res.users/sea rch",
            "/json/2/rés.users/search",
            "/json/2/res.users/séarch",
            "/json/1/res.users/search",
        ] {
            let err = Route::parse_json2(bad).unwrap_err();
            assert_eq!(err.status(), 404, "{bad}");
            assert_eq!(err.message(), DID_YOU_MEAN, "{bad}");
        }
        let long = format!("/json/2/{}/search", "a".repeat(MAX_NAME_LEN + 1));
        assert_eq!(Route::parse_json2(&long).unwrap_err().status(), 404);
        let max = format!("/json/2/{}/search", "a".repeat(MAX_NAME_LEN));
        assert!(Route::parse_json2(&max).is_ok());
    }

    #[test]
    fn call_kw_names() {
        assert_eq!(
            Route::call_kw("res.users", "read").unwrap().kind(),
            RouteKind::CallKw
        );
        assert_eq!(
            Route::call_kw("Res", "read").unwrap_err().message(),
            "the model 'Res' does not exist"
        );
        assert_eq!(
            Route::call_kw("res.users", "re-ad").unwrap_err().message(),
            "The method 'res.users.re-ad' does not exist"
        );
    }
}
