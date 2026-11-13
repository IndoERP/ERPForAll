//! Errors of the view inheritance engine.

use std::fmt;

use crate::validate::ArchIssue;

/// A resource limit of the engine (limits rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// Encoded uncompressed description response bytes.
    DescriptionEncodedBytes,
    /// Simultaneously retained owned storage while describing a request.
    DescriptionBuildBytes,
    /// Expression work steps (nodes decoded and walked) in one description request, shared by
    /// every expression, nested view and visitor of that request.
    DescriptionExpressionWork,
    /// Aggregate encoded expression bytes in one description request, shared like the work steps.
    DescriptionExpressionBytes,
    /// Size in bytes of one arch string.
    ArchBytes,
    /// Elements in one parsed arch.
    ArchElements,
    /// XML nodes in one parsed arch as the parser counts them: the document node, elements, text
    /// runs, comments and processing instructions. Bounds the parser's memory before the element
    /// count applies, so a comment- or text-heavy arch can hit it with few elements.
    ArchNodes,
    /// Attributes on one element, in a parsed arch and while combining (`position="attributes"`
    /// can add them). Attribute lookups scan an element's attributes, so this bounds each scan.
    ElementAttributes,
    /// Element nesting depth, checked after parsing and after every applied spec.
    Depth,
    /// Nodes allocated while combining one view (the inserted, copied and removed ones included).
    CombinedNodes,
    /// Bytes of tag names, attributes and text allocated while combining one view.
    CombinedBytes,
    /// Length in bytes of one XPath expression.
    XpathLength,
    /// Parenthesis / `not(` nesting inside one XPath predicate.
    XpathNesting,
    /// Location steps in one XPath expression.
    XpathSteps,
    /// Evaluation steps of one combination: nodes visited while locating spec targets (XPath
    /// steps and element locators) and by the depth check after each spec, predicate tests,
    /// attributes scanned, children visited, tokens edited by `position="attributes"`, slots of
    /// XPath mark tables, and one step per 16 bytes compared, split or searched. Bounds the
    /// specs × nodes × steps × data cost of a view.
    EvaluationSteps,
    /// Views on one `inherit_id` path, the view itself included (
    /// §12): bounds every walk up the tree.
    InheritanceDepth,
    /// Views in one catalogue (every tree a combination or a validation reads, together).
    CatalogueViews,
    /// Bytes of names, model names and archs of one catalogue's views:
    /// bounds what one view check fetches and holds (review S-3).
    CatalogueBytes,
    /// Distinct group references in all the archs one view check validates, together (each
    /// `groups` attribute is bounded by [`crate::validate::MAX_GROUP_REFS`] itself): bounds the
    /// lookup of the groups that exist (review R-1/S-1).
    CheckGroupRefs,
    /// Evaluation steps of one batch of combinations ([`crate::ViewCatalogue::combine_many`],
    /// what one view check combines), together: each combination keeps its own
    /// [`LimitKind::EvaluationSteps`] budget and draws from this one (review S-2).
    BatchSteps,
    /// Retained storage of a batch: parsed-arch cache, output arenas, dependency lists and
    /// their container capacities. The active combined arch and spec being parsed remain
    /// separately bounded by their arch limits; retention and root copying check this budget.
    BatchBytes,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ArchBytes => "arch size in bytes",
            Self::ArchElements => "elements per arch",
            Self::ArchNodes => "XML nodes per arch",
            Self::ElementAttributes => "attributes per element",
            Self::Depth => "element nesting depth",
            Self::CombinedNodes => "nodes in a combined arch",
            Self::DescriptionEncodedBytes => "encoded description bytes",
            Self::DescriptionBuildBytes => "simultaneously retained description bytes",
            Self::DescriptionExpressionWork => "expression work steps per request",
            Self::DescriptionExpressionBytes => "aggregate expression bytes per request",
            Self::CombinedBytes => "bytes in a combined arch",
            Self::XpathLength => "XPath expression length",
            Self::XpathNesting => "XPath nesting depth",
            Self::XpathSteps => "XPath steps",
            Self::EvaluationSteps => "evaluation steps per combination",
            Self::InheritanceDepth => "views on an inheritance path",
            Self::CatalogueViews => "views in one catalogue",
            Self::CatalogueBytes => "text bytes in one catalogue",
            Self::CheckGroupRefs => "group references in one view check",
            Self::BatchSteps => "evaluation steps per batch of combinations",
            Self::BatchBytes => "retained bytes per batch of combinations",
        })
    }
}

/// Why an XPath expression was rejected. The supported grammar is listed in;
/// everything else is rejected by name, never treated as a non-match.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum XpathErrorKind {
    /// The expression is empty or only whitespace.
    Empty,
    /// A construct outside the supported grammar (comparison, union, axis, `..`, ...).
    Unsupported(&'static str),
    /// A function other than `not`, `text` and `hasclass`.
    UnsupportedFunction(String),
    /// A character that starts no token of the grammar.
    UnexpectedChar(char),
    /// A token in the wrong place.
    UnexpectedToken {
        /// What the grammar expected here.
        expected: &'static str,
        /// The token found, as written.
        found: String,
    },
    /// The expression ended where the grammar expected more.
    UnexpectedEnd {
        /// What the grammar expected here.
        expected: &'static str,
    },
    /// A string literal without its closing quote.
    UnclosedString,
    /// A position index that is zero or does not fit in `usize`.
    InvalidIndex(String),
    /// `hasclass()` without class names.
    HasClassWithoutArguments,
    /// A number inside `and`/`or`/`not(...)`: a position index is only supported as a whole
    /// predicate (`[2]`).
    PositionInsideExpression,
}

impl fmt::Display for XpathErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty expression"),
            Self::Unsupported(what) => {
                write!(f, "{what} is not supported in view inheritance XPath")
            }
            Self::UnsupportedFunction(name) => {
                write!(
                    f,
                    "function '{name}()' is not supported in view inheritance XPath"
                )
            }
            Self::UnexpectedChar(c) => write!(f, "unexpected character {c:?}"),
            Self::UnexpectedToken { expected, found } => {
                write!(f, "expected {expected}, found '{found}'")
            }
            Self::UnexpectedEnd { expected } => write!(f, "expected {expected} at the end"),
            Self::UnclosedString => f.write_str("unclosed string literal"),
            Self::InvalidIndex(n) => write!(f, "invalid position index '{n}' (must be 1 or more)"),
            Self::HasClassWithoutArguments => f.write_str("hasclass() needs at least one class"),
            Self::PositionInsideExpression => f.write_str(
                "a position index is only supported as a whole predicate such as [2], \
                 not inside and/or/not()",
            ),
        }
    }
}

/// Errors of parsing, locating, applying and combining view archs.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewError {
    /// A closed description schema or its untrusted payload was refused.
    #[error("{0}")]
    InvalidDescription(String),
    // --- parsing -------------------------------------------------------------------------------
    /// The arch string is empty or only whitespace.
    #[error("Empty arch definition")]
    EmptyArch,

    /// The arch is not well-formed XML (includes unknown entities and duplicate attributes).
    #[error("XML syntax error at line {line}, column {column}: {message}")]
    XmlSyntax {
        /// Parser message.
        message: String,
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: u32,
    },

    /// The arch has a `DOCTYPE` declaration.
    #[error("DOCTYPE declarations are not allowed in view archs")]
    DoctypeNotAllowed,

    /// The arch declares or uses an XML namespace; tags and attributes would otherwise lose
    /// their prefix silently.
    #[error("XML namespaces are not supported in view archs: '{name}'")]
    NamespaceNotSupported {
        /// The prefixed name or the declared prefix.
        name: String,
    },

    /// A resource limit was exceeded.
    #[error("{limit} limit exceeded: maximum {max}{}", fmt_actual(.actual))]
    LimitExceeded {
        /// Which limit.
        limit: LimitKind,
        /// The configured maximum.
        max: usize,
        /// The value reached, when the engine knows it.
        actual: Option<usize>,
    },

    /// A host tried to set a limit to zero or above its default (limits can only be lowered).
    #[error("invalid value {value} for the {limit} limit: allowed 1 to {max}")]
    InvalidLimit {
        /// Which limit.
        limit: LimitKind,
        /// The rejected value.
        value: usize,
        /// The default, which is also the maximum.
        max: usize,
    },

    // --- locating ------------------------------------------------------------------------------
    /// An `<xpath>` spec without `expr`.
    #[error("Missing 'expr' attribute in xpath specification")]
    MissingXpathExpr,

    /// An XPath expression outside the supported grammar.
    #[error("Invalid Expression while parsing xpath “{expr}”: {kind}")]
    InvalidXpath {
        /// The expression as written.
        expr: String,
        /// Why it was rejected.
        kind: XpathErrorKind,
    },

    /// A spec uses a translated attribute (`string`, `help`, ...) as a selector.
    #[error("View inheritance may not use attribute '{0}' as a selector.")]
    TranslatedAttributeSelector(String),

    /// A `<field>` spec without `name`.
    #[error("Field specification without a 'name' attribute: {0}")]
    FieldSpecWithoutName(String),

    /// The spec matches no element of the arch.
    #[error("Element '{0}' cannot be located in parent view")]
    TargetNotFound(String),

    // --- positions -----------------------------------------------------------------------------
    /// A `position` other than `inside`, `after`, `before`, `replace`, `attributes`.
    #[error("Invalid position attribute: '{0}'")]
    InvalidPosition(String),

    /// A `mode` other than `outer` and `inner` on `position="replace"`.
    #[error("Invalid mode attribute: “{0}”")]
    InvalidMode(String),

    /// A `position="move"` spec with content.
    #[error("Invalid specification for moved nodes: “{0}”")]
    InvalidMoveSpec(String),

    /// A `position="move"` spec that matches nothing.
    #[error("Element “{0}” cannot be located in parent view")]
    MoveTargetNotFound(String),

    /// A move would take the spec's target or one of its ancestors.
    #[error("Cannot move the target node or one of its ancestors: “{0}”")]
    MoveTargetOrAncestor(String),

    /// A move would take the root element.
    #[error("Cannot move the root element: “{0}”")]
    MoveRootElement(String),

    /// `after`/`before` on the root element (XML allows one root).
    #[error("position='{0}' cannot be applied to the root element")]
    RootSiblingPosition(String),

    /// Replacing the root element needs exactly one element in the spec.
    #[error("Replacing the root element requires exactly one element, found {found}")]
    RootReplacementCount {
        /// Elements in the spec.
        found: usize,
    },

    /// `position="move"` inside a spec that replaces the root element.
    #[error("position='move' cannot be used in a specification that replaces the root element")]
    MoveInRootReplacement,

    #[error("Text {} in a position='replace' specification would be dropped", py_repr(.0))]
    ReplaceTextDropped(String),

    /// Non-whitespace text where only elements are meaningful.
    #[error("Unexpected text {} in <{container}>", py_repr(.text))]
    StrayText {
        /// The element holding the text.
        container: String,
        /// The text.
        text: String,
    },

    // --- attributes ----------------------------------------------------------------------------
    /// A child of a `position="attributes"` spec that is not `<attribute>`.
    #[error("Element with position='attributes' can only contain <attribute> tags, found <{0}>")]
    UnexpectedAttributeChild(String),

    /// An `<attribute>` with child elements.
    #[error("Element <attribute> cannot contain child elements, found <{0}>")]
    AttributeChildElement(String),

    /// An `<attribute>` without `name`.
    #[error("Element <attribute> requires a 'name' attribute")]
    AttributeWithoutName,

    /// An `<attribute>` with attributes other than `name`, `add`, `remove`, `separator`, `data-oe-*`.
    #[error("Invalid attributes {0} in element <attribute>")]
    InvalidAttributeAttributes(String),

    /// An `<attribute>` with `add`/`remove` and text.
    #[error("Element <attribute> with 'add' or 'remove' cannot contain text {}", py_repr(.0))]
    AttributeTextWithAddRemove(String),

    /// A separator other than `and`/`or` for a Python-expression attribute.
    #[error(
        "Invalid separator {} for python expression {}; valid values are 'and' and 'or'",
        py_repr(.separator),
        py_repr(.expression)
    )]
    InvalidPythonSeparator {
        /// The separator, stripped.
        separator: String,
        /// The attribute name.
        expression: String,
    },

    /// `separator=""` on a list attribute (Python's `str.split('')` raises too).
    #[error("Empty separator for attribute {}", py_repr(.0))]
    EmptySeparator(String),

    // --- combining -----------------------------------------------------------------------------
    /// Two catalogue entries with the same id.
    #[error("Duplicate view id {0} in the catalogue")]
    DuplicateViewId(i64),

    /// The requested view is not in the catalogue.
    #[error("View id {0} not found")]
    ViewNotFound(i64),

    /// A view on the requested inheritance path names a parent that is not in the catalogue.
    #[error("Parent view {parent} of view {view} not found")]
    ParentNotFound {
        /// The inheriting view.
        view: i64,
        /// The missing parent.
        parent: i64,
    },

    /// The `inherit_id` chain loops.
    #[error("Inheritance cycle through views {}", fmt_ids(.0))]
    InheritanceCycle(Vec<i64>),

    #[error("View {0} on the requested inheritance path is inactive")]
    InactiveViewOnPath(i64),

    /// An internal invariant of the engine does not hold. Never expected; report it as a bug.
    #[error("internal error in the view engine: {0}")]
    InvariantViolation(&'static str),

    /// An Elang inheritance operation failed. Ordinals are 1-based among the non-`data`
    /// operations in breadth-first container order. `line` is relative to this view's arch;
    /// the compiler resolves the ordinal to the original addon/file offset. Ordinary
    /// server-view errors retain their existing shape. The leaf kind remains in `source`.
    #[error("inheritance spec {index} at entry line {line} {spec}: {source}")]
    InSpec {
        /// The operation's 1-based ordinal in the entry.
        index: usize,
        /// The operation's 1-based line in the engine input.
        line: u32,
        /// The opening tag identifying the operation.
        spec: String,
        /// The precise parsing/application or admission failure.
        #[source]
        source: Box<ViewError>,
    },

    /// The guards of a combined arch cannot be captured (`GuardedArch::capture`):
    /// a malformed or open `groups` attribute, too many references, an unknown field. The issue
    /// carries the view and line of the node.
    #[error("{} (view {}, line {})", .0, .0.origin().view_id(), .0.origin().line())]
    GuardCapture(ArchIssue),

    /// The record guards given to `GuardedArch::capture` are not usable: a non-positive view or
    /// group id, a duplicate view, a view without groups, or too many.
    #[error("invalid record guards: {0}")]
    InvalidRecordGuards(String),

    /// An error raised while parsing or applying one view of a combination.
    #[error("error in view '{view_name}' (id {view_id})")]
    InView {
        /// The view whose arch failed.
        view_id: i64,
        /// Its name.
        view_name: String,
        /// The underlying error.
        #[source]
        source: Box<ViewError>,
    },
}

/// Specialized result alias for view operations.
pub type ViewResult<T> = Result<T, ViewError>;

fn fmt_actual(actual: &Option<usize>) -> String {
    match actual {
        Some(n) => format!(", reached {n}"),
        None => String::new(),
    }
}

fn fmt_ids(ids: &[i64]) -> String {
    let parts: Vec<String> = ids.iter().map(i64::to_string).collect();
    parts.join(" -> ")
}

pub(crate) fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_repr_matches_python() {
        assert_eq!(py_repr("text"), "'text'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("x\ny\t\\"), "'x\\ny\\t\\\\'");
        assert_eq!(py_repr("\u{1}é"), "'\\x01é'");
    }
}
