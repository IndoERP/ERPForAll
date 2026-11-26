//! Shared redirect policy separated from closed shell state.
use crate::boundary::BoundaryError;
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

const SHELL_URI_BYTES: usize = 2048;

/// The view types a page address may name (`?view_type=`): the ones the web client routes
/// (`addons/web/static/src/client/route.ts`; `tests/redirect.rs` compares the two). An address with another
/// value is refused, so a view type added to the client without its entry here fails on reload.
// shortcut: a fixed list, upgrade to the published view types of the registry when an addon can add a page view.
pub const SHELL_VIEW_TYPES: &[&str] = &[
    "list",
    "form",
    "pivot",
    "calendar",
    "graph",
    "kanban",
    "hierarchy",
    "activity",
    "map",
];

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
                "search_panel" => search_panel(value),
                "pivot_rows" | "pivot_cols" => pivot_specs(value, false),
                "pivot_measures" => pivot_specs(value, true),
                "pivot_open" => pivot_open(value),
                "pivot_sort" => pivot_sort(value),
                "calendar_scale" => matches!(value.as_str(), "day" | "week" | "month" | "year"),
                "calendar_date" => calendar_date(value),
                // The graph's state (route.ts): chart type, measure, at most two groupings, order, two switches.
                "graph_mode" => matches!(value.as_str(), "bar" | "line" | "pie"),
                "graph_measure" => pivot_field(value),
                "graph_groupby" => pivot_specs(value, false) && value.split(',').count() <= 2,
                "graph_order" => matches!(value.as_str(), "asc" | "desc" | "none"),
                "graph_stacked" | "graph_cumulated" => matches!(value.as_str(), "0" | "1"),
                "kanban_fold" => kanban_fold(value),
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
            "search_panel",
            "pivot_rows",
            "pivot_cols",
            "pivot_measures",
            "pivot_open",
            "pivot_sort",
            "calendar_scale",
            "calendar_date",
            "graph_mode",
            "graph_measure",
            "graph_groupby",
            "graph_order",
            "graph_stacked",
            "graph_cumulated",
            "kanban_fold",
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
/// A real calendar day as `YYYY-MM-DD`, years 1000 to 9999 (the client's `calendarDate` in `route.ts`).
fn calendar_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes[range.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| value[range].parse::<u32>().ok())
            .flatten()
    };
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[0] == b'0' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
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
/// Match the client’s encodeURIComponent spelling for an escaped pivot header.
fn pivot_text(encoded: &str) -> bool {
    let Ok(text) = decode(encoded) else {
        return false;
    };
    if text.encode_utf16().count() > 256 {
        return false;
    }
    let mut input = encoded.bytes();
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            if input.next() != Some(byte) {
                return false;
            }
        } else if input.next() != Some(b'%')
            || input.next() != Some(HEX[usize::from(byte >> 4)])
            || input.next() != Some(HEX[usize::from(byte & 15)])
        {
            return false;
        }
    }
    input.next().is_none()
}

fn pivot_path(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|keys| {
        keys.len() <= PIVOT_MAX_DEPTH
            && keys.iter().all(|key| match key {
                serde_json::Value::String(text) => text.encode_utf16().count() <= 256,
                serde_json::Value::Object(fields) if fields.len() == 1 => fields
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(pivot_text),
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
/// `{"<section id>": [value, ...]}`: the search panel's selection (the client's `searchPanel` in `route.ts`). At most
/// 64 sections with ids 1 to 4294967295, each at most 256 distinct values: a record id or a selection key of 1 to 256
/// UTF-16 units (the client counts the same).
fn search_panel(value: &str) -> bool {
    let Ok(serde_json::Value::Object(sections)) = serde_json::from_str::<serde_json::Value>(value)
    else {
        return false;
    };
    sections.len() <= 64
        && sections.iter().all(|(id, values)| {
            record_id(id)
                && id.parse::<u32>().is_ok()
                && values.as_array().is_some_and(|values| {
                    let mut unique = BTreeSet::new();
                    values.len() <= 256
                        && values.iter().all(|item| {
                            let ok = match item {
                                serde_json::Value::String(text) => {
                                    !text.is_empty() && text.encode_utf16().count() <= 256
                                }
                                serde_json::Value::Number(number) => number
                                    .as_u64()
                                    .is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991),
                                _ => false,
                            };
                            ok && unique.insert(item.to_string())
                        })
                })
        })
}
/// The kanban board keeps the columns its user folded or opened in the address, for one grouping
/// (M4.KANBAN-CLIENT-COLUMNS; the client's `validKanbanFold` in `kanban-fold.ts` admits the same):
/// `{"by": "<field or field:interval>", "f": [key, ...], "o": [key, ...]}`. A key is a group value: a short text
/// with nothing an address or JSON would escape, a browser-safe integer or a boolean; no key twice and none in
/// both lists; only the canonical text the client writes is admitted (no spaces, no other member order, no
/// other spelling of a number). What a field or a value means is the client's to check against the model.
const KANBAN_FOLD_UNITS: usize = 1024;
const KANBAN_FOLD_KEYS: usize = 64;
const KANBAN_FOLD_KEY_UNITS: usize = 64;
fn kanban_fold(value: &str) -> bool {
    // The client bounds the value in UTF-16 code units (a JavaScript string length).
    if value.encode_utf16().count() > KANBAN_FOLD_UNITS {
        return false;
    }
    let Ok(serde_json::Value::Object(state)) = serde_json::from_str::<serde_json::Value>(value)
    else {
        return false;
    };
    let (
        3,
        Some(serde_json::Value::String(by)),
        Some(serde_json::Value::Array(folded)),
        Some(serde_json::Value::Array(opened)),
    ) = (state.len(), state.get("by"), state.get("f"), state.get("o"))
    else {
        return false;
    };
    let grouping = match by.split_once(':') {
        None => pivot_field(by),
        Some((name, interval)) => {
            pivot_field(name) && matches!(interval, "day" | "week" | "month" | "quarter" | "year")
        }
    };
    let key = |key: &serde_json::Value| match key {
        serde_json::Value::String(text) => {
            text.encode_utf16().count() <= KANBAN_FOLD_KEY_UNITS
                && !text.contains("//")
                && !text
                    .chars()
                    .any(|c| c.is_control() || c == '\\' || c == '"')
        }
        serde_json::Value::Number(number) => number
            .as_i64()
            .is_some_and(|n| n.unsigned_abs() <= 9_007_199_254_740_991),
        serde_json::Value::Bool(_) => true,
        _ => false,
    };
    let mut seen = BTreeSet::new();
    grouping
        && folded.len() <= KANBAN_FOLD_KEYS
        && opened.len() <= KANBAN_FOLD_KEYS
        && folded
            .iter()
            .chain(opened)
            .all(|item| key(item) && seen.insert(item.to_string()))
        // Only the text the client writes: members in the order `by`, `f`, `o`, no spaces, integers written plainly.
        && format!(
            r#"{{"by":{},"f":{},"o":{}}}"#,
            serde_json::Value::String(by.clone()),
            serde_json::Value::Array(folded.clone()),
            serde_json::Value::Array(opened.clone())
        ) == value
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
