//! Raw schema declarations. Deserialization alone does not admit an executable AST.
use super::numbers;
use serde::{Deserialize, Serialize};

macro_rules! operators {
    ($wire:ident, $native:ident, $file:literal, $($variant:ident),+ $(,)?) => {
        #[doc = concat!("Wire representation of [`crate::", stringify!($native), "`].")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
        #[cfg_attr(feature = "schema-gen", ts(export_to = $file))]
        #[serde(rename_all = "snake_case")]
        #[non_exhaustive]
        pub enum $wire { $($variant),+ }
        impl From<crate::$native> for $wire {
            fn from(value: crate::$native) -> Self { match value { $(crate::$native::$variant => Self::$variant),+ } }
        }
        impl From<$wire> for crate::$native {
            fn from(value: $wire) -> Self { match value { $($wire::$variant => Self::$variant),+ } }
        }
    };
}
operators!(
    WireUnaryOp,
    UnaryOp,
    "WireUnaryOp.d.ts",
    UAdd,
    USub,
    Not,
    Invert
);
operators!(
    WireBinaryOp,
    BinaryOp,
    "WireBinaryOp.d.ts",
    Add,
    Sub,
    Mult,
    Div,
    FloorDiv,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    LShift,
    RShift
);
operators!(WireBoolOp, BoolOp, "WireBoolOp.d.ts", And, Or);
operators!(
    WireCompareOp,
    CompareOp,
    "WireCompareOp.d.ts",
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    In,
    NotIn,
    Is,
    IsNot
);

/// Untrusted versioned AST transport; use [`super::ValidatedAst`] to validate it.
///
/// Generic serde deserialization uses the caller's numeric parser. For JSON bytes, use
/// [`super::ValidatedAst::from_json`] to guarantee browser-equivalent numeric token rounding;
/// validating a raw DTO cannot recover a token already rounded by another deserializer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireDocument.d.ts"))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WireDocument {
    /// Version; currently exactly one.
    #[serde(deserialize_with = "numbers::index")]
    pub schema: u32,
    /// Postorder tree; the last entry is its root.
    #[serde(deserialize_with = "numbers::nodes")]
    pub nodes: Vec<WireNode>,
}

/// Exact scalar encoding. Integer and float payloads are strings, never JSON numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireLiteral.d.ts"))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum WireLiteral {
    // Empty struct deliberately: serde internally tagged unit variants ignore extra fields.
    None {},
    Bool {
        value: bool,
    },
    Int {
        value: String,
    },
    Float {
        value: String,
    },
    Str {
        value: String,
    },
    Bytes {
        #[serde(deserialize_with = "numbers::bytes")]
        value: Vec<u8>,
    },
}

/// Index or optional slice bounds; nullable fields must still be present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireSubscript.d.ts"))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum WireSubscript {
    Index {
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
    },
    Slice {
        #[serde(deserialize_with = "numbers::optional_index")]
        lower: Option<u32>,
        #[serde(deserialize_with = "numbers::optional_index")]
        upper: Option<u32>,
        #[serde(deserialize_with = "numbers::optional_index")]
        step: Option<u32>,
    },
}

/// Ordered keyword argument; null means dictionary unpacking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireKeyword.d.ts"))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WireKeyword {
    #[serde(deserialize_with = "numbers::required_option")]
    pub name: Option<String>,
    #[serde(deserialize_with = "numbers::index")]
    pub value: u32,
}

/// One comprehension clause, preserving target, iterable and filter order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireComprehension.d.ts"))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WireComprehension {
    #[serde(deserialize_with = "numbers::index")]
    pub target: u32,
    #[serde(deserialize_with = "numbers::index")]
    pub iter: u32,
    #[serde(deserialize_with = "numbers::indices")]
    pub ifs: Vec<u32>,
}

/// Closed transport union for every current expression node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[cfg_attr(feature = "schema-gen", ts(export_to = "WireNode.d.ts"))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum WireNode {
    Literal {
        value: WireLiteral,
    },
    Name {
        name: String,
    },
    Unary {
        op: WireUnaryOp,
        #[serde(deserialize_with = "numbers::index")]
        operand: u32,
    },
    Binary {
        op: WireBinaryOp,
        #[serde(deserialize_with = "numbers::index")]
        left: u32,
        #[serde(deserialize_with = "numbers::index")]
        right: u32,
    },
    BoolOp {
        op: WireBoolOp,
        #[serde(deserialize_with = "numbers::indices")]
        values: Vec<u32>,
    },
    Compare {
        #[serde(deserialize_with = "numbers::index")]
        left: u32,
        ops: Vec<WireCompareOp>,
        #[serde(deserialize_with = "numbers::indices")]
        comparators: Vec<u32>,
    },
    IfExp {
        #[serde(deserialize_with = "numbers::index")]
        test: u32,
        #[serde(deserialize_with = "numbers::index")]
        body: u32,
        #[serde(deserialize_with = "numbers::index")]
        orelse: u32,
    },
    Attribute {
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
        attr: String,
    },
    Subscript {
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
        slice: WireSubscript,
    },
    Call {
        #[serde(deserialize_with = "numbers::index")]
        func: u32,
        #[serde(deserialize_with = "numbers::indices")]
        args: Vec<u32>,
        keywords: Vec<WireKeyword>,
    },
    List {
        #[serde(deserialize_with = "numbers::indices")]
        items: Vec<u32>,
    },
    Tuple {
        #[serde(deserialize_with = "numbers::indices")]
        items: Vec<u32>,
    },
    Dict {
        #[serde(deserialize_with = "numbers::pairs")]
        items: Vec<(u32, u32)>,
    },
    Walrus {
        name: String,
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
    },
    JoinedStr {
        #[serde(deserialize_with = "numbers::indices")]
        items: Vec<u32>,
    },
    FormattedValue {
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
        #[serde(deserialize_with = "numbers::required_option")]
        conversion: Option<String>,
        #[serde(deserialize_with = "numbers::required_option")]
        format_spec: Option<String>,
    },
    ListComp {
        #[serde(deserialize_with = "numbers::index")]
        elt: u32,
        generators: Vec<WireComprehension>,
    },
    GeneratorExp {
        #[serde(deserialize_with = "numbers::index")]
        elt: u32,
        generators: Vec<WireComprehension>,
    },
    Starred {
        #[serde(deserialize_with = "numbers::index")]
        value: u32,
    },
}
