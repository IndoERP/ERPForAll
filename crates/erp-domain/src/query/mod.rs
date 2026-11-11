//! The query AST (spec `QAST`): one JSON document
//! describing one `search_read` or one `read_group`, parsed with strict rules, validated without a
//! registry, written in one canonical form, and run later by whoever holds the document, with
//! that principal's rights (`erp_orm::query`).
//!
//! A [`QueryAst`] has private fields and no `Deserialize`: it comes from [`QueryAst::parse`],
//! [`QueryAst::from_json`] or the builders ([`QueryAst::search_read`], [`QueryAst::read_group`]),
//! all of which run the same validation. [`QueryAst::canonical`] is its identity: the RFC 8785
//! bytes, with every key written, that parse back to an equal AST.
//!
//! ```
//! use erp_domain::query::{QueryAst, QueryLimits};
//!
//! let limits = QueryLimits::default();
//! let ast = QueryAst::parse(
//!     br#"{"version": 1, "kind": "search_read", "model": "res.partner",
//!          "domain": [["name", "ilike", "acme"]], "fields": ["name"], "limit": 20}"#,
//!     &limits,
//! )?;
//! assert_eq!(ast.model(), "res.partner");
//! let canonical = ast.canonical();
//! assert_eq!(QueryAst::parse(&canonical, &limits)?, ast);
//! # Ok::<(), erp_domain::query::QueryAstError>(())
//! ```

mod builder;
mod decode;
mod encode;
mod error;
mod granularity;
mod limits;
mod names;
mod parts;
#[cfg(test)]
mod tests;

use erp_fields::json::{self, Json};

use crate::Domain;

pub use builder::{ReadGroupBuilder, SearchReadBuilder};
pub use error::{Bound, JsonPath, QueryAstError, QueryAstReason};
pub use granularity::Granularity;
pub use limits::{QueryLimits, QueryLimitsBuilder};
pub use names::{FieldName, FieldPath, ModelName};
pub use parts::{
    Aggregate, AggregateFunc, FieldOrder, GroupBy, GroupOrder, HavingCondition, HavingOp, NullsPos,
    OrderDir, QueryContext,
};

/// The only document version this crate reads and writes.
pub const QUERY_AST_VERSION: u32 = 1;

/// A validated query document.
///
/// Equality is equality of canonical bytes: two ASTs are equal when they are the same query.
///
/// # Example
///
/// ```
/// use erp_domain::query::{AggregateFunc, QueryAst, QueryKind, QueryLimits};
///
/// let ast = QueryAst::read_group("account.move")
///     .group_by("partner_id", None)
///     .count()
///     .aggregate("amount_total", AggregateFunc::Sum)
///     .limit(50)
///     .build(&QueryLimits::default())?;
/// assert!(matches!(ast.kind(), QueryKind::ReadGroup { .. }));
/// assert_eq!(ast.limit(), 50);
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone)]
pub struct QueryAst {
    pub(crate) model: ModelName,
    pub(crate) domain: Domain,
    pub(crate) context: QueryContext,
    pub(crate) body: Body,
    pub(crate) limit: u32,
    pub(crate) offset: u32,
    /// The canonical document tree; its bytes are the AST's identity.
    pub(crate) doc: Json,
}

#[derive(Debug, Clone)]
pub(crate) enum Body {
    SearchRead {
        fields: Vec<FieldName>,
        order: Vec<FieldOrder>,
    },
    ReadGroup {
        groupby: Vec<GroupBy>,
        aggregates: Vec<Aggregate>,
        having: Vec<HavingCondition>,
        order: Vec<GroupOrder>,
    },
}

/// The kind-specific part of a [`QueryAst`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum QueryKind<'a> {
    /// `search_read`: the fields to return and the order.
    SearchRead {
        /// Fields to return (at least one).
        fields: &'a [FieldName],
        /// Order terms.
        order: &'a [FieldOrder],
    },
    /// `read_group`.
    ReadGroup {
        /// Groupings, in column order.
        groupby: &'a [GroupBy],
        /// Aggregates, in column order after the groupings.
        aggregates: &'a [Aggregate],
        /// Conditions on aggregates, ANDed.
        having: &'a [HavingCondition],
        /// Order terms over the groupings and aggregates.
        order: &'a [GroupOrder],
    },
}

impl QueryAst {
    /// Parse and validate a document.
    ///
    /// The input is checked against `limits.max_bytes` first, then read by the strict JSON parser
    /// (no duplicate keys, bounded nesting), then decoded with a closed key set per object.
    ///
    /// # Errors
    ///
    /// A [`QueryAstError`] with the JSON path of the first problem: [`QueryAstReason::Json`],
    /// [`QueryAstReason::TooLarge`], a version, key, type, name, domain or reference error.
    pub fn parse(bytes: &[u8], limits: &QueryLimits) -> Result<Self, QueryAstError> {
        if bytes.len() > limits.bytes {
            return Err(QueryAstError::new(
                &JsonPath::root(),
                QueryAstReason::TooLarge {
                    bound: Bound::Bytes,
                    max: limits::Widen::widened(limits.bytes),
                    found: bytes.len().to_string(),
                },
            ));
        }
        let doc = json::parse(bytes, limits.json_depth)
            .map_err(|e| QueryAstError::new(&JsonPath::root(), QueryAstReason::Json(e)))?;
        decode::from_json(&doc, limits)
    }

    /// Validate a document already parsed by [`erp_fields::json::parse`] (RPC and tool layers pass
    /// the subtree they hold). The same rules as [`QueryAst::parse`], without the byte check of
    /// the input.
    ///
    /// # Errors
    ///
    /// As [`QueryAst::parse`].
    pub fn from_json(doc: &Json, limits: &QueryLimits) -> Result<Self, QueryAstError> {
        decode::from_json(doc, limits)
    }

    /// Start a `search_read` query on `model`.
    pub fn search_read(model: &str) -> SearchReadBuilder {
        SearchReadBuilder::new(model)
    }

    /// Start a `read_group` query on `model`.
    pub fn read_group(model: &str) -> ReadGroupBuilder {
        ReadGroupBuilder::new(model)
    }

    /// The canonical form: RFC 8785 JSON (sorted keys, no whitespace) with every key of the kind
    /// written. It parses back to an equal AST and writes the same bytes again.
    pub fn canonical(&self) -> Vec<u8> {
        json::canonical(&self.doc)
    }

    /// The canonical document tree.
    pub fn to_json(&self) -> &Json {
        &self.doc
    }

    /// The model name.
    pub fn model(&self) -> &str {
        self.model.as_str()
    }

    /// The domain, with principal parameters as [`crate::Value::Param`] leaves.
    pub fn domain(&self) -> &Domain {
        &self.domain
    }

    /// The context keys the document carries.
    pub fn context(&self) -> &QueryContext {
        &self.context
    }

    /// The page size: rows for `search_read`, groups for `read_group`.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Records or groups skipped.
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// The kind and its parts.
    pub fn kind(&self) -> QueryKind<'_> {
        match &self.body {
            Body::SearchRead { fields, order } => QueryKind::SearchRead { fields, order },
            Body::ReadGroup {
                groupby,
                aggregates,
                having,
                order,
            } => QueryKind::ReadGroup {
                groupby,
                aggregates,
                having,
                order,
            },
        }
    }

    /// The same query with another page, validated again under `limits`.
    ///
    /// # Errors
    ///
    /// [`QueryAstReason::TooLarge`] or [`QueryAstReason::TooSmall`] at `$.limit` or `$.offset`
    /// when the page is outside the bounds.
    ///
    /// ```
    /// use erp_domain::query::{QueryAst, QueryLimits};
    ///
    /// let limits = QueryLimits::default();
    /// let first = QueryAst::search_read("res.partner").field("name").limit(20).build(&limits)?;
    /// let second = first.with_page(20, 20, &limits)?;
    /// assert_eq!((second.limit(), second.offset()), (20, 20));
    /// assert_eq!(first.model(), second.model());
    /// # Ok::<(), erp_domain::query::QueryAstError>(())
    /// ```
    pub fn with_page(
        &self,
        limit: u32,
        offset: u32,
        limits: &QueryLimits,
    ) -> Result<Self, QueryAstError> {
        let Json::Object(members) = &self.doc else {
            // the document is built as an object by `encode::document` and never replaced
            return Err(QueryAstError::new(
                &JsonPath::root(),
                QueryAstReason::WrongType {
                    expected: "object",
                    found: self.doc.type_name(),
                },
            ));
        };
        let page = |key: &str, value: u32| (Box::<str>::from(key), Json::Int(i64::from(value)));
        let mut members: Vec<(Box<str>, Json)> = members
            .iter()
            .filter(|(k, _)| &**k != "limit" && &**k != "offset")
            .cloned()
            .collect();
        members.push(page("limit", limit));
        members.push(page("offset", offset));
        decode::from_json(&Json::Object(members), limits)
    }
}

impl PartialEq for QueryAst {
    fn eq(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}

impl Eq for QueryAst {}
