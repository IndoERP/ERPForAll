//! Errors of the data-file readers: always a file, a line and a precise kind.

use std::error::Error as StdError;
use std::fmt;

use crate::ast::{InvalidXmlId, XmlIdProblem};

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// Not well-formed XML, a forbidden entity, a `DOCTYPE`, a malformed attribute.
    XmlSyntax,
    /// Element nesting deeper than the reader's depth limit ([`crate::MAX_DEPTH`] by default).
    DepthLimitExceeded {
        limit: usize,
    },
    /// One operation's source is larger than the reader's byte limit
    /// ([`crate::MAX_OPERATION_BYTES`] by default).
    OperationTooLarge {
        limit: usize,
    },
    InvalidRootTag {
        tag: String,
    },
    UnknownElement {
        parent: String,
        tag: String,
    },
    UnknownAttribute {
        tag: String,
        attr: String,
    },
    /// A required attribute is absent or empty.
    MissingRequiredAttribute {
        tag: String,
        attr: String,
    },
    /// A required child element is absent (`<bundle>` in `<asset>`, `<value>` in a list).
    MissingElement {
        parent: String,
        tag: String,
    },
    /// A child element that may appear once appears again.
    RepeatedElement {
        parent: String,
        tag: String,
    },
    /// Non-whitespace text where the schema allows none.
    UnexpectedText {
        tag: String,
    },
    /// Attributes or contents that exclude each other (`ref` with `eval`, `eval` with children…).
    ConflictingAttributes {
        tag: String,
    },
    /// An attribute value that does not parse (boolean, integer, `type`, …).
    InvalidAttributeValue {
        tag: String,
        attr: String,
    },
    /// Element text that does not parse for its type (`type="int"` …).
    InvalidValue {
        tag: String,
    },
    /// The cause ([`InvalidXmlId`]) is the error's `source()`.
    InvalidXmlId {
        value: String,
        problem: XmlIdProblem,
    },
    ForbiddenName {
        name: String,
    },
    /// Two `<value name="x">` with the same name in one `<function>`.
    DuplicateKeywordArgument {
        name: String,
    },
    Refused {
        tag: String,
    },
    /// A CSV file that cannot be read (syntax, header, row shape, external ID).
    Csv,
    /// A data file whose extension is neither `.xml` nor `.csv`.
    UnknownFileType,
}

impl ParseErrorKind {
    pub fn is_schema(&self) -> bool {
        matches!(
            self,
            Self::InvalidRootTag { .. }
                | Self::UnknownElement { .. }
                | Self::UnknownAttribute { .. }
                | Self::MissingRequiredAttribute { .. }
                | Self::MissingElement { .. }
                | Self::RepeatedElement { .. }
                | Self::UnexpectedText { .. }
                | Self::ConflictingAttributes { .. }
        )
    }

    /// True for value errors inside an operation, reported with the operation's source.
    pub(crate) fn wants_node(&self) -> bool {
        matches!(
            self,
            Self::InvalidAttributeValue { .. }
                | Self::InvalidValue { .. }
                | Self::InvalidXmlId { .. }
                | Self::ForbiddenName { .. }
                | Self::DuplicateKeywordArgument { .. }
                | Self::Refused { .. }
        )
    }
}

/// Node source kept in an error is capped to this many bytes (plus "…").
pub const NODE_TEXT_LIMIT: usize = 2048;

/// A data-file error with its file, line and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    inner: Box<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Inner {
    file: String,
    line: usize,
    kind: ParseErrorKind,
    message: String,
    node: Option<String>,
    cause: Option<InvalidXmlId>,
}

impl ParseError {
    pub(crate) fn new(
        file: &str,
        line: usize,
        kind: ParseErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            inner: Box::new(Inner {
                file: file.to_string(),
                line,
                kind,
                message: message.into(),
                node: None,
                cause: None,
            }),
        }
    }

    pub(crate) fn syntax(file: &str, line: usize, message: impl Into<String>) -> Self {
        Self::new(file, line, ParseErrorKind::XmlSyntax, message)
    }

    pub(crate) fn csv(file: &str, line: usize, message: impl Into<String>) -> Self {
        Self::new(file, line, ParseErrorKind::Csv, message)
    }

    /// An invalid external ID; `context` says where it was written, the cause says why.
    pub(crate) fn invalid_xml_id(
        file: &str,
        line: usize,
        context: impl Into<String>,
        cause: InvalidXmlId,
    ) -> Self {
        let kind = ParseErrorKind::InvalidXmlId {
            value: cause.value().to_string(),
            problem: cause.problem(),
        };
        let mut err = Self::new(file, line, kind, context);
        err.inner.cause = Some(cause);
        err
    }

    /// Attach the operation's source, capped to [`NODE_TEXT_LIMIT`] bytes.
    pub(crate) fn with_node(mut self, node_source: &str) -> Self {
        let trimmed = node_source.trim_end();
        let text = if trimmed.len() > NODE_TEXT_LIMIT {
            let mut cut = NODE_TEXT_LIMIT;
            while !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            let mut capped = String::with_capacity(cut + 3);
            capped.push_str(&trimmed[..cut]);
            capped.push('…');
            capped
        } else {
            trimmed.to_string()
        };
        self.inner.node = Some(text);
        self
    }

    pub fn file(&self) -> &str {
        &self.inner.file
    }

    pub fn line(&self) -> usize {
        self.inner.line
    }

    pub fn kind(&self) -> &ParseErrorKind {
        &self.inner.kind
    }

    /// The error text, without the node and without the cause (see [`ParseError::detail`]).
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// The message followed by the cause, as shown after the location in `Display`.
    pub fn detail(&self) -> String {
        match &self.inner.cause {
            Some(cause) => format!("{}: {cause}", self.inner.message),
            None => self.inner.message.clone(),
        }
    }

    /// Source of the operation the error occurred in (capped), for value errors.
    pub fn node(&self) -> Option<&str> {
        self.inner.node.as_deref()
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Inner {
            file,
            line,
            kind,
            node,
            ..
        } = &*self.inner;
        let message = self.detail();
        if kind.is_schema() {
            write!(
                f,
                "The XML file '{file}' does not fit the required schema!\n{file}:{line}: {message}"
            )
        } else if let Some(node) = node {
            write!(
                f,
                "while parsing {file}:{line}, somewhere inside\n{node}\n{message}"
            )
        } else {
            write!(f, "while parsing {file}:{line}: {message}")
        }
    }
}

impl StdError for ParseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .cause
            .as_ref()
            .map(|c| c as &(dyn StdError + 'static))
    }
}
