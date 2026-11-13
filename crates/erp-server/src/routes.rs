// SPDX-License-Identifier: MIT OR Apache-2.0
//! `scripts/kg-build.py` reads this file as text (plain string literals, one table entry per
//! line) so `kg.py check-client-calls` can fail on a client URL literal that no route serves.
//! Keep the `ROUTES`, `LEGACY_EXACT` and `LEGACY_PREFIX` entries in the literal form below.
//!
//! The table is a tooling contract: `scripts/kg-build.py` reads this file, which is why its items are `pub`
//! and kept as plain literals.
//!
//! The table comes from the handlers in `web.rs`, not from alias targets: an alias whose
//! target has no handler (see [`KNOWN_UNSERVED_ALIAS_TARGETS`]) is not a served route.

/// How one table entry matches a canonical request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMatch {
    /// The path equals the text.
    Exact(&'static str),
    /// The path starts with the text.
    Prefix(&'static str),
    /// The path contains the text anywhere (the `/static/` asset branch).
    Segment(&'static str),
}

/// Site root, redirected to [`ERP`].
pub const ROOT: &str = "/";
/// Web client shell.
pub const ERP: &str = "/erp";
/// Prefix of action-addressed shell URLs.
pub const ACTION_PREFIX: &str = "/erp/action-";
/// Sign-in page and form post.
pub const LOGIN: &str = "/erp/login";
/// Landing page of a signed-in user without web client access.
pub const LOGIN_SUCCESS: &str = "/erp/login_successful";
/// Sign-out.
pub const LOGOUT: &str = "/erp/session/logout";
/// Readiness probe, matched on the raw path.
pub const HEALTH_READY: &str = "/erp/health/ready";
/// Browser error report endpoint.
pub const CLIENT_ERROR: &str = "/erp/client-error";
/// Module lifecycle: install.
pub const MODULE_INSTALL: &str = "/erp/module/install";
/// Module lifecycle: upgrade.
pub const MODULE_UPGRADE: &str = "/erp/module/upgrade";
/// Module lifecycle: uninstall.
pub const MODULE_UNINSTALL: &str = "/erp/module/uninstall";
/// Module lifecycle: force demo data.
pub const MODULE_FORCE_DEMO: &str = "/erp/module/force-demo";
/// Favicon probed by browsers.
pub const FAVICON: &str = "/favicon.ico";
/// Brand icon.
pub const BRAND_ICON_PATH: &str = "/erp/brand/icon.svg";
/// Brand logo.
pub const BRAND_LOGO_PATH: &str = "/erp/brand/logo.svg";
/// Brand logo for dark backgrounds.
pub const BRAND_LOGO_DARK_PATH: &str = "/erp/brand/logo-dark.svg";
/// Prefix of published web client assets.
pub const ASSETS_PREFIX: &str = "/erp/assets/";
/// Prefix of the JSON-2 RPC surface.
pub const JSON2_PREFIX: &str = "/erp/json/2/";
/// Prefix of the authenticated attachment content route (`/erp/content/<id>`).
pub const CONTENT_PREFIX: &str = "/erp/content/";
/// The pivot view's spreadsheet download (`POST`).
pub const PIVOT_EXPORT: &str = "/erp/pivot/export";
/// Text that selects the addon static asset branch.
pub const STATIC_SEGMENT: &str = "/static/";

/// Every path the router serves, canonical form (after the legacy rewrite).
pub const ROUTES: &[RouteMatch] = &[
    RouteMatch::Exact("/"),
    RouteMatch::Exact("/erp"),
    RouteMatch::Prefix("/erp/action-"),
    RouteMatch::Exact("/erp/login"),
    RouteMatch::Exact("/erp/login_successful"),
    RouteMatch::Exact("/erp/session/logout"),
    RouteMatch::Exact("/erp/health/ready"),
    RouteMatch::Exact("/erp/client-error"),
    RouteMatch::Exact("/erp/pivot/export"),
    RouteMatch::Exact("/erp/module/install"),
    RouteMatch::Exact("/erp/module/upgrade"),
    RouteMatch::Exact("/erp/module/uninstall"),
    RouteMatch::Exact("/erp/module/force-demo"),
    RouteMatch::Exact("/favicon.ico"),
    RouteMatch::Exact("/erp/brand/icon.svg"),
    RouteMatch::Exact("/erp/brand/logo.svg"),
    RouteMatch::Exact("/erp/brand/logo-dark.svg"),
    RouteMatch::Prefix("/erp/assets/"),
    RouteMatch::Prefix("/erp/json/2/"),
    RouteMatch::Prefix("/erp/content/"),
    RouteMatch::Segment("/static/"),
];

/// Legacy paths rewritten when the whole path equals the left side: `(legacy, native)`.
pub const LEGACY_EXACT: &[(&str, &str)] = &[
    ("/odoo", "/erp"),
    ("/web", "/erp"),
    ("/web/login", "/erp/login"),
    ("/web/login_successful", "/erp/login_successful"),
    ("/web/session/logout", "/erp/session/logout"),
    ("/web/module/install", "/erp/module/install"),
];

/// Legacy prefixes rewritten keeping the suffix: `(legacy prefix, native prefix)`.
pub const LEGACY_PREFIX: &[(&str, &str)] = &[
    ("/odoo/action-", "/erp/action-"),
    ("/web/assets/", "/erp/assets/"),
    ("/web/content/", "/erp/content/"),
    ("/json/2/", "/erp/json/2/"),
    ("/web/dataset/call_kw", "/erp/dataset/call_kw"),
];

/// Alias targets that no handler serves today. A test pins this list so it cannot grow silently.
pub const KNOWN_UNSERVED_ALIAS_TARGETS: &[&str] = &["/erp/dataset/call_kw"];

/// Whether a canonical path (already through the legacy rewrite) is served by some route.
#[must_use]
pub fn served(canonical: &str) -> bool {
    ROUTES.iter().any(|route| match *route {
        RouteMatch::Exact(text) => canonical == text,
        RouteMatch::Prefix(text) => canonical.starts_with(text),
        RouteMatch::Segment(text) => canonical.contains(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMED: &[&str] = &[
        ROOT,
        ERP,
        LOGIN,
        LOGIN_SUCCESS,
        LOGOUT,
        HEALTH_READY,
        CLIENT_ERROR,
        PIVOT_EXPORT,
        MODULE_INSTALL,
        MODULE_UPGRADE,
        MODULE_UNINSTALL,
        MODULE_FORCE_DEMO,
        FAVICON,
        BRAND_ICON_PATH,
        BRAND_LOGO_PATH,
        BRAND_LOGO_DARK_PATH,
    ];

    #[test]
    fn every_exact_entry_is_served() {
        for route in ROUTES {
            if let RouteMatch::Exact(text) = route {
                assert!(served(text), "{text}");
            }
        }
    }

    #[test]
    fn samples_under_prefix_and_segment_are_served() {
        for route in ROUTES {
            match route {
                RouteMatch::Prefix(text) => assert!(served(&format!("{text}sample")), "{text}"),
                RouteMatch::Segment(text) => {
                    assert!(served(&format!("/erp/x{text}y.png")), "{text}");
                }
                RouteMatch::Exact(_) => {}
            }
        }
    }

    #[test]
    fn named_consts_are_in_the_table() {
        for text in NAMED {
            assert!(ROUTES.contains(&RouteMatch::Exact(text)), "{text}");
        }
        for (text, wanted) in [
            (ACTION_PREFIX, "prefix"),
            (ASSETS_PREFIX, "prefix"),
            (JSON2_PREFIX, "prefix"),
            (CONTENT_PREFIX, "prefix"),
            (STATIC_SEGMENT, "segment"),
        ] {
            let found = ROUTES.iter().any(|route| match (route, wanted) {
                (RouteMatch::Prefix(t), "prefix") | (RouteMatch::Segment(t), "segment") => {
                    *t == text
                }
                _ => false,
            });
            assert!(found, "{text}");
        }
    }

    #[test]
    fn alias_targets_are_served_or_known_unserved() {
        let mut unserved: Vec<String> = Vec::new();
        for (_, native) in LEGACY_EXACT {
            if !served(native) {
                unserved.push((*native).to_owned());
            }
        }
        for (_, native) in LEGACY_PREFIX {
            if !served(&format!("{native}sample")) && !served(native) {
                unserved.push((*native).to_owned());
            }
        }
        assert_eq!(unserved, KNOWN_UNSERVED_ALIAS_TARGETS);
    }

    #[test]
    fn unrouted_paths_are_not_served() {
        for path in ["/web/nope/3", "/erp/dataset/call_kw", "/erp/nope", "/erp/"] {
            assert!(!served(path), "{path}");
        }
    }
}
