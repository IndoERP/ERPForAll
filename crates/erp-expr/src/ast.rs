//! Abstract syntax tree for Python expressions.

/// A literal scalar value in the AST.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Literal {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnaryOp {
    /// `+`
    UAdd,
    /// `-`
    USub,
    /// `not`
    Not,
    /// `~`
    Invert,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mult,
    /// `/`
    Div,
    /// `//`
    FloorDiv,
    /// `%`
    Mod,
    /// `**`
    Pow,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `<<`
    LShift,
    /// `>>`
    RShift,
}

/// Boolean logical operators with short-circuit semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BoolOp {
    And,
    Or,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompareOp {
    /// `==`
    Eq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtE,
    /// `>`
    Gt,
    /// `>=`
    GtE,
    /// `in`
    In,
    /// `not in`
    NotIn,
    /// `is`
    Is,
    /// `is not`
    IsNot,
}

/// Subscript indexing or slice.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SubscriptKind {
    /// Single index `[idx]`
    Index(Box<Expr>),
    /// Slice `[lower:upper:step]`
    Slice {
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
}

/// A parsed expression node.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Expr {
    /// A literal constant (`1`, `2.5`, `"hello"`, `True`, `None`).
    Literal(Literal),

    /// An identifier reference (`x`, `ref`, `Command`).
    Name(String),

    /// Unary operation (`-x`, `not flag`).
    Unary { op: UnaryOp, operand: Box<Expr> },

    /// Binary operation (`a + b`, `x * y`).
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },

    /// Boolean operations (`a and b and c`, `x or y`).
    BoolOp { op: BoolOp, values: Vec<Expr> },

    /// Chained comparisons (`a == b`, `1 < x <= 10`).
    Compare {
        left: Box<Expr>,
        ops: Vec<CompareOp>,
        comparators: Vec<Expr>,
    },

    /// Conditional ternary (`x if cond else y`).
    IfExp {
        test: Box<Expr>,
        body: Box<Expr>,
        orelse: Box<Expr>,
    },

    /// Attribute access (`obj.field`).
    Attribute { value: Box<Expr>, attr: String },

    /// Subscripting or slicing (`obj[0]`, `obj['key']`, `s[1:5]`).
    Subscript {
        value: Box<Expr>,
        slice: SubscriptKind,
    },

    /// Function or method call (`fn(arg1, arg2, kw=val)`).
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        keywords: Vec<(Option<String>, Expr)>,
    },

    /// List literal (`[1, 2, 3]`).
    List(Vec<Expr>),

    /// Tuple literal (`(1, 2)` or `(1,)` or `()`).
    Tuple(Vec<Expr>),

    /// Dict literal (`{'k': v, 1: 2}`).
    Dict(Vec<(Expr, Expr)>),

    /// Walrus assignment expression (`name := value`).
    Walrus { name: String, value: Box<Expr> },

    /// Formatted string literal / f-string parts.
    JoinedStr(Vec<Expr>),

    /// Formatted value within an f-string (`{expr:spec}`).
    FormattedValue {
        value: Box<Expr>,
        conversion: Option<char>,
        format_spec: Option<String>,
    },

    /// List comprehension `[elt for ...]`
    ListComp {
        elt: Box<Expr>,
        generators: Vec<Comprehension>,
    },

    /// Generator expression `(elt for ...)`
    GeneratorExp {
        elt: Box<Expr>,
        generators: Vec<Comprehension>,
    },

    /// Starred expression (`*args`).
    Starred(Box<Expr>),
}

/// A comprehension clause: `for target in iter (if cond)*`
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Comprehension {
    pub target: Box<Expr>,
    pub iter: Box<Expr>,
    pub ifs: Vec<Expr>,
}
