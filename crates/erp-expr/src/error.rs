//! Error type for parsing and evaluation.
//!
//! Resource limits
//! have their own `LimitExceeded` class: Python has no equivalent.

/// An error raised while parsing or evaluating an expression. Never a panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExprError {
    /// The source is not valid in the supported Python-expression subset.
    #[error("SyntaxError: {message} (line {line}, column {col})")]
    Parse {
        message: String,
        offset: usize,
        line: usize,
        col: usize,
    },

    /// A construct that parses but is not allowed at evaluation time (for example rebinding a
    /// comprehension variable with `:=`).
    #[error("SyntaxError: {message}")]
    Syntax { message: String },

    #[error("TypeError: {message}")]
    Type { message: String },

    #[error("NameError: name '{name}' is not defined")]
    Name { name: String },

    /// The standard
    /// context never invents it; `provider` names who supplies it.
    #[error("NameError: name '{name}' is not available in this context: {provider}")]
    Unavailable { name: String, provider: String },

    #[error("AttributeError: {message}")]
    Attribute { message: String },

    /// `key` is the Python `repr()` of the missing key.
    #[error("KeyError: {key}")]
    Key { key: String },

    #[error("IndexError: {message}")]
    Index { message: String },

    #[error("ZeroDivisionError: {message}")]
    ZeroDivision { message: String },

    /// Integer results outside the 64-bit range and out-of-range dates or floats.
    #[error("OverflowError: {message}")]
    Overflow { message: String },

    #[error("ValueError: {message}")]
    Value { message: String },

    /// A resource limit was reached.
    #[error("LimitExceeded: {message}")]
    LimitExceeded { message: String },

    /// Invalid view-modifier attribute combination.
    #[error("ModifierError: {message}")]
    Modifier { message: String },
}

impl ExprError {
    pub fn parse(message: impl Into<String>, offset: usize, line: usize, col: usize) -> Self {
        Self::Parse {
            message: message.into(),
            offset,
            line,
            col,
        }
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::Syntax {
            message: message.into(),
        }
    }

    pub fn type_error(message: impl Into<String>) -> Self {
        Self::Type {
            message: message.into(),
        }
    }

    pub fn name_error(name: impl Into<String>) -> Self {
        Self::Name { name: name.into() }
    }

    pub fn unavailable(name: impl Into<String>, provider: impl Into<String>) -> Self {
        Self::Unavailable {
            name: name.into(),
            provider: provider.into(),
        }
    }

    /// `'<type>' object has no attribute '<attr>'`.
    pub fn attribute_error(
        type_name: impl std::fmt::Display,
        attr: impl std::fmt::Display,
    ) -> Self {
        Self::Attribute {
            message: format!("'{type_name}' object has no attribute '{attr}'"),
        }
    }

    /// `type object '<type>' has no attribute '<attr>'` (attributes of classes and namespaces).
    pub fn type_attribute_error(
        type_name: impl std::fmt::Display,
        attr: impl std::fmt::Display,
    ) -> Self {
        Self::Attribute {
            message: format!("type object '{type_name}' has no attribute '{attr}'"),
        }
    }

    /// `module '<module>' has no attribute '<attr>'`.
    pub fn module_attribute_error(
        module: impl std::fmt::Display,
        attr: impl std::fmt::Display,
    ) -> Self {
        Self::Attribute {
            message: format!("module '{module}' has no attribute '{attr}'"),
        }
    }

    pub fn key_error(key_repr: impl Into<String>) -> Self {
        Self::Key {
            key: key_repr.into(),
        }
    }

    pub fn index_error(message: impl Into<String>) -> Self {
        Self::Index {
            message: message.into(),
        }
    }

    pub fn zero_division(message: impl Into<String>) -> Self {
        Self::ZeroDivision {
            message: message.into(),
        }
    }

    pub fn overflow(message: impl Into<String>) -> Self {
        Self::Overflow {
            message: message.into(),
        }
    }

    pub fn value_error(message: impl Into<String>) -> Self {
        Self::Value {
            message: message.into(),
        }
    }

    pub fn limit(message: impl Into<String>) -> Self {
        Self::LimitExceeded {
            message: message.into(),
        }
    }

    pub fn modifier_error(message: impl Into<String>) -> Self {
        Self::Modifier {
            message: message.into(),
        }
    }

    /// The Python exception class name of this error (`"TypeError"`, `"LimitExceeded"`, ...).
    pub fn class(&self) -> &'static str {
        match self {
            Self::Parse { .. } | Self::Syntax { .. } => "SyntaxError",
            Self::Type { .. } => "TypeError",
            Self::Name { .. } | Self::Unavailable { .. } => "NameError",
            Self::Attribute { .. } => "AttributeError",
            Self::Key { .. } => "KeyError",
            Self::Index { .. } => "IndexError",
            Self::ZeroDivision { .. } => "ZeroDivisionError",
            Self::Overflow { .. } => "OverflowError",
            Self::Value { .. } => "ValueError",
            Self::LimitExceeded { .. } => "LimitExceeded",
            Self::Modifier { .. } => "ModifierError",
        }
    }
}

/// Result alias for expression operations.
pub type ExprResult<T> = Result<T, ExprError>;
