//! View-type registry declarations (H18).
//!
//! Pure, database-free registration descriptors for view types (list, form, search, etc.).

use crate::view_type_decl::located;
use crate::{ViewResult, ViewTypeDecl};

/// Compatibility table mapping reserved bare view-type keys to their declared owner addon.
const RESERVED_BARE_VIEW_TYPES: &[(&str, &str)] = &[
    ("list", "web"),
    ("form", "web"),
    ("search", "web"),
    ("kanban", "web"),
    ("graph", "web"),
    ("pivot", "web"),
    ("calendar", "web"),
    ("card", "web"),
    ("activity", "mail"),
    ("hierarchy", "web_hierarchy"),
    ("gantt", "web_gantt"),
    ("grid", "web_grid"),
    ("map", "web_map"),
    ("cohort", "web_cohort"),
];

/// Validate an identifier component: ASCII `[a-z][a-z0-9_]*`, bounded to 64 bytes.
pub(crate) fn is_valid_ident(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Validate a view-type key: bare key or `<addon>.<name>`.
pub fn is_valid_view_type_key(key: &str) -> bool {
    if let Some((addon, name)) = key.split_once('.') {
        is_valid_ident(addon) && is_valid_ident(name) && !name.contains('.')
    } else {
        is_valid_ident(key)
    }
}

/// The addon that owns a reserved bare view-type key, if the key is reserved.
pub(crate) fn reserved_owner(key: &str) -> Option<&'static str> {
    RESERVED_BARE_VIEW_TYPES
        .iter()
        .find(|(bare, _)| *bare == key)
        .map(|&(_, owner)| owner)
}

/// Validate a set of view-type registrations.
///
/// Each [`ViewTypeDecl`] was already checked by its builder (identifier and key syntax, wire major,
/// owner and namespace rules, mandatory schema/validator/builder/metadata field set). What only a
/// set can decide is left here: no key is registered twice, including by the same owner.
pub fn validate_view_types(types: &[&ViewTypeDecl]) -> ViewResult<()> {
    for (i, decl) in types.iter().enumerate() {
        if types[i + 1..].iter().any(|other| other.key() == decl.key()) {
            return Err(located(
                "registers a duplicate key",
                decl.addon(),
                decl.key(),
            ));
        }
    }
    Ok(())
}

/// Look up a view type in the registered set by key.
pub fn find_view_type<'a>(types: &[&'a ViewTypeDecl], key: &str) -> Option<&'a ViewTypeDecl> {
    types.iter().copied().find(|decl| decl.key() == key)
}
