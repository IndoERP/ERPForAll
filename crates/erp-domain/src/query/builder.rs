//! Builders for query ASTs in Rust. They collect the same document the decoder reads and call the
//! same validation in `build`, so a built AST and a parsed one cannot differ in what they accept.

use erp_fields::json::Json;

use crate::Granularity;

use super::error::QueryAstError;
use super::limits::QueryLimits;
use super::parts::{AggregateFunc, HavingOp, NullsPos, OrderDir};
use super::{QueryAst, decode};

fn member(key: &str, value: Json) -> (Box<str>, Json) {
    (Box::from(key), value)
}

fn text(value: &str) -> Json {
    Json::Str(Box::from(value))
}

fn int(value: u32) -> Json {
    Json::Int(i64::from(value))
}

fn order_term(key_name: &str, key: &str, dir: OrderDir, nulls: Option<NullsPos>) -> Json {
    Json::Object(vec![
        member(key_name, text(key)),
        member("dir", text(dir.as_str())),
        member("nulls", nulls.map_or(Json::Null, |n| text(n.as_str()))),
    ])
}

/// What both kinds share.
#[derive(Debug, Clone)]
struct Common {
    model: String,
    domain: Option<Json>,
    active_test: Option<bool>,
    limit: Option<u32>,
    offset: Option<u32>,
    order: Vec<Json>,
}

impl Common {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            domain: None,
            active_test: None,
            limit: None,
            offset: None,
            order: Vec::new(),
        }
    }

    /// The document members both kinds carry.
    fn members(&self, kind: &str) -> Vec<(Box<str>, Json)> {
        let mut members = vec![
            member("version", Json::Int(1)),
            member("kind", text(kind)),
            member("model", text(&self.model)),
            member("order", Json::Array(self.order.clone())),
        ];
        if let Some(domain) = &self.domain {
            members.push(member("domain", domain.clone()));
        }
        if let Some(flag) = self.active_test {
            members.push(member(
                "context",
                Json::Object(vec![member("active_test", Json::Bool(flag))]),
            ));
        }
        if let Some(limit) = self.limit {
            members.push(member("limit", int(limit)));
        }
        if let Some(offset) = self.offset {
            members.push(member("offset", int(offset)));
        }
        members
    }
}

/// Builds a `search_read` [`QueryAst`]; see [`QueryAst::search_read`].
///
/// `limit` and at least one field are required, as in a document; `build` reports a missing one
/// with the same error a document without it gets.
///
/// ```
/// use erp_domain::query::{OrderDir, QueryAst, QueryLimits};
/// use erp_fields::json::Json;
///
/// let domain = Json::Array(vec![Json::Array(vec![
///     Json::Str("state".into()),
///     Json::Str("=".into()),
///     Json::Str("posted".into()),
/// ])]);
/// let ast = QueryAst::search_read("account.move")
///     .domain(domain)
///     .fields(["name", "amount_total"])
///     .order("invoice_date", OrderDir::Desc, None)
///     .limit(80)
///     .build(&QueryLimits::default())?;
/// assert_eq!(ast.limit(), 80);
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SearchReadBuilder {
    common: Common,
    fields: Vec<String>,
}

impl SearchReadBuilder {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            common: Common::new(model),
            fields: Vec::new(),
        }
    }

    /// Absent: every record.
    #[must_use]
    pub fn domain(mut self, domain: Json) -> Self {
        self.common.domain = Some(domain);
        self
    }

    /// Add a field to return.
    #[must_use]
    pub fn field(mut self, name: &str) -> Self {
        self.fields.push(name.to_owned());
        self
    }

    /// Add fields to return.
    #[must_use]
    pub fn fields<'a>(mut self, names: impl IntoIterator<Item = &'a str>) -> Self {
        self.fields.extend(names.into_iter().map(str::to_owned));
        self
    }

    /// Add an order term.
    #[must_use]
    pub fn order(mut self, field: &str, dir: OrderDir, nulls: Option<NullsPos>) -> Self {
        self.common
            .order
            .push(order_term("field", field, dir, nulls));
        self
    }

    /// Set `active_test` in the context.
    #[must_use]
    pub fn active_test(mut self, flag: bool) -> Self {
        self.common.active_test = Some(flag);
        self
    }

    /// Set the page size (rows).
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.common.limit = Some(limit);
        self
    }

    /// Set the number of rows skipped.
    #[must_use]
    pub fn offset(mut self, offset: u32) -> Self {
        self.common.offset = Some(offset);
        self
    }

    /// Validate and build.
    ///
    /// # Errors
    ///
    /// A [`QueryAstError`] as [`QueryAst::from_json`] gives for the equivalent document.
    pub fn build(&self, limits: &QueryLimits) -> Result<QueryAst, QueryAstError> {
        let mut members = self.common.members("search_read");
        members.push(member(
            "fields",
            Json::Array(self.fields.iter().map(|f| text(f)).collect()),
        ));
        decode::from_json(&Json::Object(members), limits)
    }
}

/// Builds a `read_group` [`QueryAst`]; see [`QueryAst::read_group`].
///
/// ```
/// use erp_domain::Granularity;
/// use erp_domain::query::{AggregateFunc, HavingOp, OrderDir, QueryAst, QueryLimits};
/// use erp_fields::json::Json;
///
/// let ast = QueryAst::read_group("account.move")
///     .group_by("invoice_date", Some(Granularity::Month))
///     .count()
///     .aggregate("amount_total", AggregateFunc::Sum)
///     .having("amount_total:sum", HavingOp::Gt, Json::Int(1000))
///     .order("amount_total:sum", OrderDir::Desc, None)
///     .limit(500)
///     .build(&QueryLimits::default())?;
/// assert_eq!(ast.limit(), 500);
/// # Ok::<(), erp_domain::query::QueryAstError>(())
/// ```
#[derive(Debug, Clone)]
pub struct ReadGroupBuilder {
    common: Common,
    groupby: Vec<Json>,
    aggregates: Vec<Json>,
    having: Vec<Json>,
}

impl ReadGroupBuilder {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            common: Common::new(model),
            groupby: Vec::new(),
            aggregates: Vec::new(),
            having: Vec::new(),
        }
    }

    /// The domain; see [`SearchReadBuilder::domain`].
    #[must_use]
    pub fn domain(mut self, domain: Json) -> Self {
        self.common.domain = Some(domain);
        self
    }

    /// Add a grouping on a field path (`partner_id`, `partner_id.country_id`).
    #[must_use]
    pub fn group_by(mut self, path: &str, granularity: Option<Granularity>) -> Self {
        self.groupby.push(Json::Object(vec![
            member("path", text(path)),
            member(
                "granularity",
                granularity.map_or(Json::Null, |g| text(g.name())),
            ),
        ]));
        self
    }

    /// Add the record count of each group (`__count`).
    #[must_use]
    pub fn count(mut self) -> Self {
        self.aggregates.push(Json::Object(vec![
            member("func", text("count")),
            member("field", Json::Null),
        ]));
        self
    }

    /// Add an aggregate of a field.
    #[must_use]
    pub fn aggregate(mut self, field: &str, func: AggregateFunc) -> Self {
        self.aggregates.push(Json::Object(vec![
            member("func", text(func.as_str())),
            member("field", text(field)),
        ]));
        self
    }

    /// Add a condition on an aggregate, named by its spec name (`amount_total:sum`). The value is
    /// a scalar, or a list of scalars for `in` and `not in`.
    #[must_use]
    pub fn having(mut self, aggregate: &str, op: HavingOp, value: Json) -> Self {
        self.having.push(Json::Object(vec![
            member("aggregate", text(aggregate)),
            member("op", text(op.as_str())),
            member("value", value),
        ]));
        self
    }

    /// Add an order term over a grouping or aggregate, named by its spec name.
    #[must_use]
    pub fn order(mut self, key: &str, dir: OrderDir, nulls: Option<NullsPos>) -> Self {
        self.common.order.push(order_term("key", key, dir, nulls));
        self
    }

    /// Set `active_test` in the context.
    #[must_use]
    pub fn active_test(mut self, flag: bool) -> Self {
        self.common.active_test = Some(flag);
        self
    }

    /// Set the page size (groups).
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.common.limit = Some(limit);
        self
    }

    /// Set the number of groups skipped.
    #[must_use]
    pub fn offset(mut self, offset: u32) -> Self {
        self.common.offset = Some(offset);
        self
    }

    /// Validate and build.
    ///
    /// # Errors
    ///
    /// A [`QueryAstError`] as [`QueryAst::from_json`] gives for the equivalent document.
    pub fn build(&self, limits: &QueryLimits) -> Result<QueryAst, QueryAstError> {
        let mut members = self.common.members("read_group");
        members.push(member("groupby", Json::Array(self.groupby.clone())));
        members.push(member("aggregates", Json::Array(self.aggregates.clone())));
        members.push(member("having", Json::Array(self.having.clone())));
        decode::from_json(&Json::Object(members), limits)
    }
}
