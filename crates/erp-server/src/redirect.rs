//! Shared redirect policy separated from closed shell state.
use crate::boundary::BoundaryError;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

const SHELL_URI_BYTES: usize = 2048;

/// The view types a page address may name (`?view_type=`): the ones the web client routes
/// (`addons/web/static/src/client/route.ts`; `tests/redirect.rs` compares the two). An address with another
/// value is refused, so a view type added to the client without its entry here fails on reload.
// shortcut: a fixed list, upgrade to the published view types of the registry when an addon can add a page view.
pub const SHELL_VIEW_TYPES: &[&str] = &["list", "form", "pivot"];

/// Startup-bound canonical origin and registered view capabilities.
#[derive(Debug)]
pub struct RedirectPolicy {
    origin: Url,
    views: BTreeSet<String>,
}
impl RedirectPolicy {
    /// Construct policy using the trusted canonical origin and registered view keys.
    /// # Errors
    /// Invalid origin or registry keys.
    pub fn new(origin: &str, views: &[&str]) -> Result<Self, BoundaryError> {
        let origin = Url::parse(origin).map_err(|_| BoundaryError::Redirect)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || views.is_empty()
            || views.len() > 256
        {
            return Err(BoundaryError::Redirect);
        }
        let mut registry = BTreeSet::new();
        for view in views {
            if !identifier(view) || !registry.insert((*view).to_owned()) {
                return Err(BoundaryError::Redirect);
            }
        }
        Ok(Self {
            origin,
            views: registry,
        })
    }
    /// Normalize safe redirect transport; this does not authorize the destination route.
    /// # Errors
    /// External, ambiguous or malformed URL. Percent decoding is inspected exactly once.
    pub fn normalize(&self, value: &str) -> Result<String, BoundaryError> {
        if value.is_empty()
            || value.len() > SHELL_URI_BYTES
            || value
                .chars()
                .any(|c| c.is_control() || c == '\\' || c == ' ')
        {
            return Err(BoundaryError::Redirect);
        }
        let decoded = decode(value)?;
        if decoded.starts_with("//")
            || decoded.chars().any(|c| c.is_control() || c == '\\')
            || value.to_ascii_lowercase().contains("%2f%2f")
        {
            return Err(BoundaryError::Redirect);
        }
        let parsed = if value.starts_with('/') {
            if value.starts_with("//") {
                return Err(BoundaryError::Redirect);
            }
            self.origin.join(value)
        } else {
            let rest = value
                .strip_prefix("https://")
                .or_else(|| value.strip_prefix("http://"))
                .ok_or(BoundaryError::Redirect)?;
            if rest.is_empty() || rest.starts_with('/') {
                return Err(BoundaryError::Redirect);
            }
            Url::parse(value)
        }
        .map_err(|_| BoundaryError::Redirect)?;
        if parsed.origin() != self.origin.origin()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(BoundaryError::Redirect);
        }
        // Resolution removes dot segments (`/.//x`, `/a/..//x`, `%2e`) and keeps an empty first
        // segment (`https://origin//x`), so the input checks above cannot see every route to a
        // path a browser reads as scheme-relative. Check what is actually returned.
        if parsed.path().starts_with("//") || decode(parsed.path())?.starts_with("//") {
            return Err(BoundaryError::Redirect);
        }
        Ok(parsed[url::Position::BeforePath..].to_owned())
    }
    /// Admit's closed shell state and canonicalize aliases/query ordering.
    /// Search values are transport hints; authorized view/search services validate their semantics.
    /// View keys come from registration. No URL value grants action, company or session authority.
    /// # Errors
    /// Malformed shell state or unregistered view key. Record/menu/company access is checked separately.
    pub fn shell(&self, value: &str) -> Result<String, BoundaryError> {
        let local = self.normalize(value)?;
        if local.contains('#') {
            return Err(BoundaryError::Redirect);
        }
        let (path, query) = local.split_once('?').unwrap_or((&local, ""));
        let path = decode(path)?;
        let mut state = BTreeMap::new();
        if !query.is_empty() {
            for pair in query.split('&') {
                let (key, value) = pair.split_once('=').ok_or(BoundaryError::Redirect)?;
                let key = decode(&key.replace('+', " "))?;
                let value = decode(&value.replace('+', " "))?;
                if value.is_empty() || state.insert(key, value).is_some() {
                    return Err(BoundaryError::Redirect);
                }
            }
        }
        let action = if matches!(path.as_str(), "/erp" | "/odoo" | "/web") {
            state.remove("action")
        } else if let Some(action) = path
            .strip_prefix("/erp/action-")
            .or_else(|| path.strip_prefix("/odoo/action-"))
        {
            if state.contains_key("action") {
                return Err(BoundaryError::Redirect);
            }
            Some(action.to_owned())
        } else {
            return Err(BoundaryError::Redirect);
        };
        if action.as_deref().is_some_and(|id| !record_id(id)) {
            return Err(BoundaryError::Redirect);
        }
        for (key, value) in &state {
            if action.is_none() && key != "cids" && key != "debug" {
                return Err(BoundaryError::Redirect);
            }
            let accepted = match key.as_str() {
                "menu" | "id" | "active_id" => record_id(value),
                "model" => model_name(value),
                "view_type" => self.views.contains(value),
                "debug" => value == "1",
                "search" | "search_facets" | "search_filters" | "search_groupby" | "list_tab" => {
                    // A raw slash pair would serialize into the encoded pair that normalize
                    // deliberately refuses. Refuse before publishing a non-reloadable URL.
                    value.len() <= SHELL_URI_BYTES && !value.contains("//")
                }
                "pivot_rows" | "pivot_cols" => pivot_specs(value, false),
                "pivot_measures" => pivot_specs(value, true),
                "pivot_open" => pivot_open(value),
                "pivot_sort" => pivot_sort(value),
                "method_action" => method_identity(value),
                "cids" => {
                    let parts: Vec<_> = value.split(',').collect();
                    let mut unique = BTreeSet::new();
                    parts.len() <= 64 && parts.iter().all(|id| record_id(id) && unique.insert(*id))
                }
                _ => false,
            };
            if !accepted {
                return Err(BoundaryError::Redirect);
            }
        }
        if state.contains_key("id") && state.get("view_type").map(String::as_str) != Some("form") {
            return Err(BoundaryError::Redirect);
        }
        let mut result = action.map_or_else(|| "/erp".to_owned(), |id| format!("/erp/action-{id}"));
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for key in [
            "debug",
            "menu",
            "model",
            "id",
            "view_type",
            "active_id",
            "search",
            "search_facets",
            "search_filters",
            "search_groupby",
            "method_action",
            "list_tab",
            "pivot_rows",
            "pivot_cols",
            "pivot_measures",
            "pivot_open",
            "pivot_sort",
            "cids",
        ] {
            if let Some(value) = state.get(key) {
                query.append_pair(key, value);
            }
        }
        let query = query.finish();
        if !query.is_empty() {
            result.push('?');
            result.push_str(&query);
        }
        if result.len() > SHELL_URI_BYTES {
            return Err(BoundaryError::Redirect);
        }
        Ok(result)
    }
}
fn model_name(value: &str) -> bool {
    value.len() <= 128
        && value.split('.').all(|part| {
            let mut bytes = part.bytes();
            bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}
/// The pivot view keeps its state in the address. The shapes are closed and bounded here; what a
/// field name or a group value means is the client's to check against the model.
const PIVOT_MAX_MEASURES: usize = 16;
/// Row or column groupings per axis: the grouped read crosses every depth of one axis with every depth of the other,
/// and one call takes at most 64 grouping sets (7 x 7 = 49 here with the totals).
const PIVOT_MAX_LEVELS: usize = 6;
const PIVOT_MAX_PATHS: usize = 1024;
const PIVOT_MAX_DEPTH: usize = 16;
fn pivot_field(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 128
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// A comma list of `field` or `field:interval` (or `__count` among measures), no repeats; at most 6 groupings or 16 measures.
fn pivot_specs(value: &str, measures: bool) -> bool {
    let parts: Vec<_> = value.split(',').collect();
    let mut unique = BTreeSet::new();
    parts.len()
        <= if measures {
            PIVOT_MAX_MEASURES
        } else {
            PIVOT_MAX_LEVELS
        }
        && parts.iter().all(|part| {
            let ok = match part.split_once(':') {
                None => pivot_field(part),
                Some((name, interval)) => {
                    !measures
                        && pivot_field(name)
                        && matches!(interval, "day" | "week" | "month" | "quarter" | "year")
                }
            };
            ok && unique.insert(*part)
        })
}
fn pivot_path(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|keys| {
        keys.len() <= PIVOT_MAX_DEPTH
            && keys.iter().all(|key| match key {
                serde_json::Value::String(text) => text.len() <= 256,
                serde_json::Value::Number(number) => number
                    .as_i64()
                    .is_some_and(|n| n.unsigned_abs() <= 9_007_199_254_740_991),
                serde_json::Value::Bool(_) => true,
                _ => false,
            })
    })
}
/// `{"r": [path, ...], "c": [path, ...]}`: the headers kept expanded on the row and the column axis.
fn pivot_open(value: &str) -> bool {
    let Ok(serde_json::Value::Object(axes)) = serde_json::from_str::<serde_json::Value>(value)
    else {
        return false;
    };
    let mut paths = 0;
    axes.keys().all(|key| matches!(key.as_str(), "r" | "c"))
        && axes.values().all(|list| {
            list.as_array().is_some_and(|list| {
                paths += list.len();
                paths <= PIVOT_MAX_PATHS && list.iter().all(pivot_path)
            })
        })
}
/// `["<measure>", "asc" | "desc", path]`: the measure column the rows are sorted by; `[]` is "sorted by nothing".
fn pivot_sort(value: &str) -> bool {
    let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(value)
    else {
        return false;
    };
    matches!(parts.as_slice(), [])
        || matches!(parts.as_slice(), [serde_json::Value::String(measure), serde_json::Value::String(order), path]
        if (measure == "__count" || pivot_field(measure)) && matches!(order.as_str(), "asc" | "desc") && pivot_path(path))
}
fn method_identity(value: &str) -> bool {
    let Some((uuid, counter)) = value.split_once(':') else {
        return false;
    };
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            14 => byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
        && !counter.starts_with('0')
        && counter.len() <= 16
        && counter.bytes().all(|byte| byte.is_ascii_digit())
        && counter
            .parse::<u64>()
            .is_ok_and(|value| value > 0 && value <= 9_007_199_254_740_991)
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
}
fn record_id(value: &str) -> bool {
    !value.starts_with('0')
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<i64>().is_ok_and(|id| id > 0)
}
fn decode(value: &str) -> Result<String, BoundaryError> {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = *bytes.get(index + 1).ok_or(BoundaryError::Redirect)?;
            let lo = *bytes.get(index + 2).ok_or(BoundaryError::Redirect)?;
            let digit = |byte: u8| {
                char::from(byte)
                    .to_digit(16)
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or(BoundaryError::Redirect)
            };
            result.push(digit(hi)? * 16 + digit(lo)?);
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(result).map_err(|_| BoundaryError::Redirect)
}
