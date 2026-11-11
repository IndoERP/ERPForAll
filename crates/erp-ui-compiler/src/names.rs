//! Validated names: addon, template, template-path and custom-directive names (inputs), and the
//! name grammars the code generator checks before a name reaches a generated module (
//! codegen safety, T-TPL.2).

use std::borrow::Borrow;
use std::fmt;

/// Why a name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NameError {
    /// An addon name outside `[A-Za-z0-9_]+` (at most 256 bytes): the grammar of
    /// `erp_modules::ModuleName`.
    #[error("invalid addon name {0:?}: expected 1 to 256 characters from [A-Za-z0-9_]")]
    Addon(String),
    /// A template name outside `[A-Za-z0-9_.:-]+` (at most 256 bytes, no leading or trailing `.`).
    #[error(
        "invalid template name {0:?}: expected 1 to 256 characters from [A-Za-z0-9_.:-], \
         not starting or ending with '.'"
    )]
    Template(String),
    /// A template file path that is not relative, `/`-separated and free of control characters.
    #[error(
        "invalid template path {path:?}: {reason} (a path is relative, '/'-separated, at most 1024 \
         bytes, with no empty, '.' or '..' segment, no '\\' and no control character)"
    )]
    Path {
        /// The refused path.
        path: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A custom directive name outside `[a-z0-9_]+` (at most 64 bytes).
    #[error("invalid custom directive name {0:?}: expected 1 to 64 characters from [a-z0-9_]")]
    Directive(String),
    /// More custom directives than [`crate::MAX_CUSTOM_DIRECTIVES`].
    #[error("more than {max} custom directives: {name:?} is one too many")]
    TooManyDirectives {
        /// The directive that would pass the maximum.
        name: String,
        /// The maximum.
        max: usize,
    },
}

/// An addon's technical name, as in its manifest: `[A-Za-z0-9_]+`, at most 256 bytes, the same
/// grammar as `erp_modules::ModuleName` (pinned by `tests/names_pin.rs`), so every installed
/// module converts.
///
/// ```
/// use erp_ui_compiler::AddonName;
///
/// assert_eq!(AddonName::new("web")?.as_str(), "web");
/// assert!(AddonName::new("web-x").is_err());
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AddonName(Box<str>);

impl AddonName {
    /// Validates an addon name.
    ///
    /// # Errors
    /// [`NameError::Addon`] when the name is empty, longer than 256 bytes or uses another character.
    pub fn new(name: &str) -> Result<Self, NameError> {
        let ok = !name.is_empty()
            && name.len() <= 256
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if ok {
            Ok(Self(name.into()))
        } else {
            Err(NameError::Addon(name.to_owned()))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AddonName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A template name (`t-name`, `t-inherit`, `t-call`): `[A-Za-z0-9_.:-]+`, at most 256 bytes, not
/// starting or ending with `.`.
///
/// ```
/// use erp_ui_compiler::TemplateName;
///
/// assert_eq!(TemplateName::new("web.ListRenderer")?.as_str(), "web.ListRenderer");
/// assert!(TemplateName::new("web.List Renderer").is_err());
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TemplateName(Box<str>);

impl TemplateName {
    /// Validates a template name.
    ///
    /// # Errors
    /// [`NameError::Template`] when the name breaks the grammar above.
    pub fn new(name: &str) -> Result<Self, NameError> {
        let ok = !name.is_empty()
            && name.len() <= 256
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
        if ok {
            Ok(Self(name.into()))
        } else {
            Err(NameError::Template(name.to_owned()))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TemplateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Maps keyed by template name can be searched with a `&str` (equality, order and hash are the
/// string's).
impl Borrow<str> for TemplateName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// The path of a template file inside its addon: relative, `/`-separated, at most 1024 bytes, no
/// empty, `.` or `..` segment, no `\` and no control character (C0, DEL, C1, U+2028, U+2029). It
/// is written into reports and canonical output, so it can never break a line.
///
/// ```
/// use erp_ui_compiler::TemplatePath;
///
/// assert_eq!(TemplatePath::new("static/src/x.xml")?.as_str(), "static/src/x.xml");
/// assert!(TemplatePath::new("../x.xml").is_err());
/// assert!(TemplatePath::new("static/src/x.xml\ntemplate").is_err());
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TemplatePath(Box<str>);

impl TemplatePath {
    /// Validates a template path.
    ///
    /// # Errors
    /// [`NameError::Path`] naming the rule the path breaks.
    pub fn new(path: &str) -> Result<Self, NameError> {
        let refuse = |reason| {
            Err(NameError::Path {
                path: path.to_owned(),
                reason,
            })
        };
        if path.is_empty() {
            return refuse("empty");
        }
        if path.len() > 1024 {
            return refuse("longer than 1024 bytes");
        }
        if path.starts_with('/') {
            return refuse("absolute");
        }
        if path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\\'))
        {
            return refuse("a control character or '\\'");
        }
        if path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return refuse("an empty, '.' or '..' segment");
        }
        Ok(Self(path.into()))
    }

    /// The path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TemplatePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A custom directive name (`t-custom-<name>`, registered by an addon in `register(ctx)`, design
/// §4.1): `[a-z0-9_]+`, at most 64 bytes.
///
/// ```
/// use erp_ui_compiler::DirectiveName;
///
/// assert_eq!(DirectiveName::new("click")?.as_str(), "click");
/// assert!(DirectiveName::new("Click").is_err());
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirectiveName(Box<str>);

impl DirectiveName {
    /// Validates a custom directive name.
    ///
    /// # Errors
    /// [`NameError::Directive`] when the name is empty, longer than 64 bytes or uses another
    /// character.
    pub fn new(name: &str) -> Result<Self, NameError> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if ok {
            Ok(Self(name.into()))
        } else {
            Err(NameError::Directive(name.to_owned()))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DirectiveName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for DirectiveName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// JavaScript reserved words and the names that may not be bound in strict-mode module code.
const RESERVED: &[&str] = &[
    "arguments",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "eval",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Whether `name` is an ASCII JavaScript identifier (`[A-Za-z_$][A-Za-z0-9_$]*`) that is not a
/// reserved word. Every user-chosen name that the code generator turns into an identifier passes
/// this check first.
pub(crate) fn is_plain_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_' || first == b'$')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        && name.len() <= 128
        && !RESERVED.contains(&name)
        && name != PROTO_KEY
}

/// The attribute names the code generator emits: `[A-Za-z_][A-Za-z0-9_.:-]*`, at most 128 bytes
/// (a subset of the HTML attribute-name production with no quote, space, `/`, `=` or `>`).
pub(crate) fn is_attribute_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        && name.len() <= 128
}

/// Event names after `t-on-`: `[a-z][a-z0-9_:-]*`, at most 64 bytes (T-TPL.2; `.` separates the
/// modifiers and is not part of the name).
pub(crate) fn is_event_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'-')
        })
        && name.len() <= 64
}

/// Element names the code generator emits: `[A-Za-z][A-Za-z0-9-]*` (HTML and SVG element names),
/// at most 64 bytes.
pub(crate) fn is_element_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && name.len() <= 64
}

/// Component tag names: `[A-Z][A-Za-z0-9_]*`, at most 128 bytes.
pub(crate) fn is_component_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_uppercase()
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && name.len() <= 128
}

/// Anchor names (`t-anchor`): `[a-z0-9_.@-]+`, at most 128 bytes.
pub(crate) fn is_anchor_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'@' | b'-')
        })
}

/// The one key no generated name may be: in an object literal, `__proto__: v` sets the prototype
/// instead of a property (props, slots and locals all become object keys at some point).
pub(crate) const PROTO_KEY: &str = "__proto__";

/// Slot names (`t-set-slot`, `t-slot`): `[A-Za-z_][A-Za-z0-9_.-]*`, at most 128 bytes, not
/// `__proto__`.
pub(crate) fn is_slot_name(name: &str) -> bool {
    if name == PROTO_KEY {
        return false;
    }
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && name.len() <= 128
}

/// Prop names on components: `[A-Za-z_$][A-Za-z0-9_$-]*`, at most 128 bytes, not `__proto__`
/// (emitted as string keys).
pub(crate) fn is_prop_name(name: &str) -> bool {
    if name == PROTO_KEY {
        return false;
    }
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_' || first == b'$')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'-'))
        && name.len() <= 128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addon_names() {
        assert!(AddonName::new("web").is_ok());
        assert!(AddonName::new("l10n_th").is_ok());
        assert!(AddonName::new("Web").is_ok());
        assert!(AddonName::new(&"a".repeat(256)).is_ok());
        for bad in ["", "web-x", "web.x", "wéb", "a b", &"a".repeat(257)] {
            assert_eq!(
                AddonName::new(bad).unwrap_err(),
                NameError::Addon(bad.to_owned()),
                "{bad:?}"
            );
        }
        assert_eq!(
            NameError::Addon("web-x".into()).to_string(),
            "invalid addon name \"web-x\": expected 1 to 256 characters from [A-Za-z0-9_]"
        );
    }

    #[test]
    fn template_names() {
        for good in [
            "web.ListRenderer",
            "web.ActionDialog.header",
            "x-y_z.1",
            "web.TreeEditor.condition:readonly",
        ] {
            assert!(TemplateName::new(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".web",
            "web.",
            "web list",
            "web\"x",
            "wéb",
            &"a".repeat(257),
        ] {
            assert_eq!(
                TemplateName::new(bad).unwrap_err(),
                NameError::Template(bad.to_owned()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn template_paths() {
        for good in [
            "static/src/x.xml",
            "a.xml",
            "static/src/my file.xml",
            "a/b-c_d.e/f.xml",
        ] {
            assert_eq!(TemplatePath::new(good).unwrap().as_str(), good);
        }
        for (bad, reason) in [
            ("", "empty"),
            ("/abs.xml", "absolute"),
            ("a//b.xml", "an empty, '.' or '..' segment"),
            ("a/./b.xml", "an empty, '.' or '..' segment"),
            ("a/../b.xml", "an empty, '.' or '..' segment"),
            ("a/", "an empty, '.' or '..' segment"),
            ("a\nb.xml", "a control character or '\\'"),
            ("a\rb.xml", "a control character or '\\'"),
            ("a\u{85}b.xml", "a control character or '\\'"),
            ("a\u{2028}b.xml", "a control character or '\\'"),
            ("a\\b.xml", "a control character or '\\'"),
        ] {
            assert_eq!(
                TemplatePath::new(bad).unwrap_err(),
                NameError::Path {
                    path: bad.to_owned(),
                    reason
                },
                "{bad:?}"
            );
        }
        let long = "a".repeat(1025);
        assert!(matches!(
            TemplatePath::new(&long),
            Err(NameError::Path {
                reason: "longer than 1024 bytes",
                ..
            })
        ));
    }

    #[test]
    fn directive_names() {
        assert!(DirectiveName::new("click").is_ok());
        assert!(DirectiveName::new("on_2").is_ok());
        for bad in ["", "Click", "a-b", "a.b", &"a".repeat(65)] {
            assert_eq!(
                DirectiveName::new(bad).unwrap_err(),
                NameError::Directive(bad.to_owned()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn identifiers_refuse_reserved_words_and_non_ascii() {
        assert!(is_plain_identifier("record"));
        assert!(is_plain_identifier("$x_1"));
        for bad in [
            "",
            "1a",
            "class",
            "this",
            "eval",
            "arguments",
            "a-b",
            "ä",
            "a b",
        ] {
            assert!(!is_plain_identifier(bad), "{bad:?}");
        }
    }

    #[test]
    fn attribute_and_event_names() {
        assert!(is_attribute_name("data-tooltip-template"));
        assert!(is_attribute_name("aria-label"));
        assert!(!is_attribute_name("x\" onload=\"alert(1)"));
        assert!(!is_attribute_name("a>b"));
        assert!(is_event_name("click"));
        assert!(is_event_name("o-custom:event"));
        assert!(!is_event_name("click);alert(1);("));
        assert!(!is_event_name("Click"));
        assert!(is_component_name("Dropdown"));
        assert!(!is_component_name("dropdown"));
        assert!(is_anchor_name("list.row.actions@2"));
        assert!(!is_anchor_name("List"));
        assert!(is_element_name("linearGradient"));
        assert!(!is_element_name("a:b"));
    }
}
