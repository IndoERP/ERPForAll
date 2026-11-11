//! Bounds on a query document: on by default, and a host can
//! only lower them.

use super::error::{Bound, JsonPath, QueryAstError, QueryAstReason};

/// The ceilings. A host may ask for lower values ([`QueryLimits::builder`]), never higher ones.
pub(crate) const DEFAULT_BYTES: usize = 65_536;
pub(crate) const DEFAULT_JSON_DEPTH: usize = 32;
pub(crate) const DEFAULT_DOMAIN_ITEMS: usize = 512;
pub(crate) const DEFAULT_DOMAIN_DEPTH: usize = 16;
pub(crate) const DEFAULT_FIELDS: usize = 128;
pub(crate) const DEFAULT_GROUPBY: usize = 6;
pub(crate) const DEFAULT_AGGREGATES: usize = 32;
pub(crate) const DEFAULT_ORDER_TERMS: usize = 8;
pub(crate) const DEFAULT_HAVING: usize = 32;
pub(crate) const DEFAULT_PATH_SEGMENTS: usize = 4;
pub(crate) const DEFAULT_ROWS: u32 = 2_000;
pub(crate) const DEFAULT_GROUPS: u32 = 10_000;
pub(crate) const DEFAULT_OFFSET: u32 = 1_000_000;

/// Bounds on the size and shape of a query document, checked when it is parsed or built.
///
/// The defaults are the ceilings: input 64 KiB, JSON depth 32, 512 domain
/// conditions, domain depth 16, 128 `fields`, 6 group-bys, 32 aggregates, 8 order terms, 32 `having`
/// conditions, 4 path segments, `limit` 2,000 rows or 10,000 groups, `offset` 1,000,000. Use
/// [`QueryLimits::builder`] to lower some of them; a value above its default, or 0, is refused.
///
/// ```
/// use erp_domain::query::{QueryAst, QueryLimits};
///
/// let limits = QueryLimits::builder().max_rows(100).build()?;
/// let text = br#"{"version":1,"kind":"search_read","model":"res.partner","fields":["name"],"limit":101}"#;
/// let err = QueryAst::parse(text, &limits).unwrap_err();
/// assert_eq!(err.to_string(), "query AST $.limit: limit is 101; at most 100 allowed");
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueryLimits {
    pub(crate) bytes: usize,
    pub(crate) json_depth: usize,
    pub(crate) domain_items: usize,
    pub(crate) domain_depth: usize,
    pub(crate) fields: usize,
    pub(crate) groupby: usize,
    pub(crate) aggregates: usize,
    pub(crate) order_terms: usize,
    pub(crate) having: usize,
    pub(crate) path_segments: usize,
    pub(crate) rows: u32,
    pub(crate) groups: u32,
    pub(crate) offset: u32,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            bytes: DEFAULT_BYTES,
            json_depth: DEFAULT_JSON_DEPTH,
            domain_items: DEFAULT_DOMAIN_ITEMS,
            domain_depth: DEFAULT_DOMAIN_DEPTH,
            fields: DEFAULT_FIELDS,
            groupby: DEFAULT_GROUPBY,
            aggregates: DEFAULT_AGGREGATES,
            order_terms: DEFAULT_ORDER_TERMS,
            having: DEFAULT_HAVING,
            path_segments: DEFAULT_PATH_SEGMENTS,
            rows: DEFAULT_ROWS,
            groups: DEFAULT_GROUPS,
            offset: DEFAULT_OFFSET,
        }
    }
}

impl QueryLimits {
    /// Input/canonical-form byte ceiling shared by narrower query consumers.
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
    /// Domain depth ceiling shared by narrower query consumers.
    pub const fn domain_depth(&self) -> usize {
        self.domain_depth
    }
    /// Leaf-condition ceiling shared by narrower query consumers.
    pub const fn domain_items(&self) -> usize {
        self.domain_items
    }
    /// Referenced-field ceiling shared by narrower query consumers.
    pub const fn fields(&self) -> usize {
        self.fields
    }
    /// Effective order-term ceiling shared by narrower query consumers.
    pub const fn order_terms(&self) -> usize {
        self.order_terms
    }

    /// A builder starting from the defaults.
    pub fn builder() -> QueryLimitsBuilder {
        QueryLimitsBuilder::default()
    }
}

/// Builds [`QueryLimits`]; see [`QueryLimits::builder`].
#[derive(Debug, Clone, Default)]
pub struct QueryLimitsBuilder {
    max_bytes: Option<usize>,
    max_json_depth: Option<usize>,
    max_domain_items: Option<usize>,
    max_domain_depth: Option<usize>,
    max_fields: Option<usize>,
    max_groupby: Option<usize>,
    max_aggregates: Option<usize>,
    max_order_terms: Option<usize>,
    max_having: Option<usize>,
    max_path_segments: Option<usize>,
    max_rows: Option<u32>,
    max_groups: Option<u32>,
    max_offset: Option<u32>,
}

impl QueryLimitsBuilder {
    /// Most bytes of an input document or canonical form.
    #[must_use]
    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = Some(n);
        self
    }

    /// Deepest JSON nesting.
    #[must_use]
    pub fn max_json_depth(mut self, n: usize) -> Self {
        self.max_json_depth = Some(n);
        self
    }

    /// Most domain conditions, those of `any` sub-domains included.
    #[must_use]
    pub fn max_domain_items(mut self, n: usize) -> Self {
        self.max_domain_items = Some(n);
        self
    }

    /// Deepest parsed domain.
    #[must_use]
    pub fn max_domain_depth(mut self, n: usize) -> Self {
        self.max_domain_depth = Some(n);
        self
    }

    /// Most entries of `fields`.
    #[must_use]
    pub fn max_fields(mut self, n: usize) -> Self {
        self.max_fields = Some(n);
        self
    }

    /// Most group-bys.
    #[must_use]
    pub fn max_groupby(mut self, n: usize) -> Self {
        self.max_groupby = Some(n);
        self
    }

    /// Most aggregates.
    #[must_use]
    pub fn max_aggregates(mut self, n: usize) -> Self {
        self.max_aggregates = Some(n);
        self
    }

    /// Most `order` terms.
    #[must_use]
    pub fn max_order_terms(mut self, n: usize) -> Self {
        self.max_order_terms = Some(n);
        self
    }

    /// Most `having` conditions.
    #[must_use]
    pub fn max_having(mut self, n: usize) -> Self {
        self.max_having = Some(n);
        self
    }

    /// Most segments of a group-by path.
    #[must_use]
    pub fn max_path_segments(mut self, n: usize) -> Self {
        self.max_path_segments = Some(n);
        self
    }

    /// Largest `limit` of a `search_read` query.
    #[must_use]
    pub fn max_rows(mut self, n: u32) -> Self {
        self.max_rows = Some(n);
        self
    }

    /// Largest `limit` of a `read_group` query.
    #[must_use]
    pub fn max_groups(mut self, n: u32) -> Self {
        self.max_groups = Some(n);
        self
    }

    /// Largest `offset`.
    #[must_use]
    pub fn max_offset(mut self, n: u32) -> Self {
        self.max_offset = Some(n);
        self
    }

    /// The limits.
    ///
    /// # Errors
    ///
    /// [`QueryAstReason::LimitNotLowerable`] for the first value that is 0 or above its default.
    pub fn build(self) -> Result<QueryLimits, QueryAstError> {
        let d = QueryLimits::default();
        Ok(QueryLimits {
            bytes: pick(Bound::Bytes, self.max_bytes, d.bytes)?,
            json_depth: pick(Bound::JsonDepth, self.max_json_depth, d.json_depth)?,
            domain_items: pick(Bound::DomainItems, self.max_domain_items, d.domain_items)?,
            domain_depth: pick(Bound::DomainDepth, self.max_domain_depth, d.domain_depth)?,
            fields: pick(Bound::Fields, self.max_fields, d.fields)?,
            groupby: pick(Bound::GroupBy, self.max_groupby, d.groupby)?,
            aggregates: pick(Bound::Aggregates, self.max_aggregates, d.aggregates)?,
            order_terms: pick(Bound::OrderTerms, self.max_order_terms, d.order_terms)?,
            having: pick(Bound::Having, self.max_having, d.having)?,
            path_segments: pick(Bound::PathSegments, self.max_path_segments, d.path_segments)?,
            rows: pick(Bound::Rows, self.max_rows, d.rows)?,
            groups: pick(Bound::Groups, self.max_groups, d.groups)?,
            offset: pick(Bound::Offset, self.max_offset, d.offset)?,
        })
    }
}

/// A bound as a `u64`, for messages.
pub(crate) trait Widen: Copy {
    fn widened(self) -> u64;
}

impl Widen for u32 {
    fn widened(self) -> u64 {
        u64::from(self)
    }
}

impl Widen for usize {
    fn widened(self) -> u64 {
        // usize is at most 64 bits on every supported target (wasm32 and 64-bit hosts)
        self as u64
    }
}

/// The requested value, or the default; a request of 0 or above the default is an error.
fn pick<T: Widen + PartialOrd>(
    bound: Bound,
    wanted: Option<T>,
    default: T,
) -> Result<T, QueryAstError> {
    let Some(found) = wanted else {
        return Ok(default);
    };
    if found > default || found.widened() == 0 {
        return Err(QueryAstError::new(
            &JsonPath::root(),
            QueryAstReason::LimitNotLowerable {
                bound,
                default: default.widened(),
                found: found.widened(),
            },
        ));
    }
    Ok(found)
}
