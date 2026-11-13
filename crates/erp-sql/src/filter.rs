//! Domain → SQL `WHERE` predicate (SPEC-DOM §4.10,…047, REL §4.14).
//!
//! This
//! module only turns the result into SQL. Compilation has two steps so that the text can be
//! memoized safely:
//!
//! 1. [`plan_where`] walks the domain once and makes every value-dependent decision: which NULL
//!    branch a condition needs, whether a set still has values once `False` is taken out, whether an
//!    inequality accepts NULL rows, which tables are joined … It returns a [`ShapeKey`] (a strict,
//!    literal-free description of the SQL text: scopes, joins, field ids, operators, decisions and
//!    parameter types) and the parameters, in placeholder order.
//! 2. [`render_where`] turns a [`ShapeKey`] into SQL text (the `WHERE` predicate and the joins of
//!    the main table). Two domains with the same key produce the same text with different
//!    parameters, so the text is memoized per key and literals never take part in the key.
//!
//! Relational conditions (REL §4.14, DOM §4.10):
//!
//! | Condition | SQL |
//! |---|---|
//! | many2one `any!` (bypass) | `JOIN`/`LEFT JOIN comodel AS <alias>` and, for a nullable column, `(f IS NOT NULL AND <alias>.id IS NOT NULL AND <sub>)` |
//! | many2one `any` (access rules apply) | `f IN (SELECT co.id FROM co WHERE <sub>)` |
//! | one2many `any` / `not any` | `[NOT] EXISTS(SELECT FROM (SELECT co.inverse AS __inverse FROM co WHERE <sub>) AS __sub WHERE __inverse = t.id)` |
//! | `many2one_reference` `any!` with a [`crate::reference::ReferenceQuery`] | `f IN (SELECT t.id FROM t WHERE <sub>)` on the query's target model (Amendment 1) |
//! | many2many `any` / `not any` | `[NOT] EXISTS (SELECT 1 FROM rel AS <alias> WHERE <alias>.c1 = t.id [AND <alias>.c2 IN (SELECT co.id FROM co WHERE <sub>)])` |
//!
//! Aliases are
//! `<source alias>__<field>`, shortened to PostgreSQL's 63 bytes (contract).

use std::sync::Arc;

use erp_domain::{
    Condition, Domain, DomainKind, NowKind, OpaqueKind, Operator, PrincipalParam, UserPath, Value,
};
use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};

use crate::param::{Param, Scalar, ScalarKind, falsy_value, to_scalar};
use crate::reference::ReferenceQuery;
use crate::relational::HierarchyStart;
use crate::{quote, shorten_identifier};

/// Name of the immutable `unaccent` wrapper created at schema sync, usable in index expressions.
pub const UNACCENT_WRAPPER: &str = "erp_unaccent";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Unaccent {
    #[default]
    Off,
    /// `unaccent()` exists but is not immutable: used in conditions, not in indexes.
    Present,
    /// The immutable [`UNACCENT_WRAPPER`] exists: used in conditions and in trigram indexes, so
    /// both use the same expression.
    Indexable,
}

impl Unaccent {
    /// `expr` wrapped in the folding function (unchanged when off).
    pub fn wrap(self, expr: &str) -> String {
        match self {
            Self::Off => expr.to_owned(),
            Self::Present => format!("unaccent({expr})"),
            Self::Indexable => format!("{UNACCENT_WRAPPER}({expr})"),
        }
    }
}

/// How a "now" principal parameter is rendered: the transaction's start time in UTC as a
/// `timestamp`, or its date (the transaction's start, the same value as audit
/// timestamps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NowCast {
    /// `(now() AT TIME ZONE 'UTC')::date`.
    Date,
    /// `(now() AT TIME ZONE 'UTC')`.
    Timestamp,
}

/// The SQL of "now" as `cast`.
pub fn now_sql(cast: NowCast) -> String {
    match cast {
        NowCast::Date => format!("{}::date", crate::NOW_UTC),
        NowCast::Timestamp => crate::NOW_UTC.to_owned(),
    }
}

/// What the compiler needs besides the domain.
#[derive(Debug, Clone, Copy)]
pub struct WhereContext<'a> {
    /// The registry (comodels of relational conditions).
    pub registry: &'a Registry,
    /// The model searched; its table is the main table of the query.
    pub model: &'a ModelSpec,
    /// Accent folding of `ilike`.
    pub unaccent: Unaccent,
    /// Context language for translated fields.
    pub lang: Option<&'a str>,
}

impl<'a> WhereContext<'a> {
    /// A filter context using the default language.
    pub fn new(registry: &'a Registry, model: &'a ModelSpec, unaccent: Unaccent) -> Self {
        Self {
            registry,
            model,
            unaccent,
            lang: None,
        }
    }

    /// Select a language; compilation validates its syntax and length.
    pub fn with_lang(mut self, lang: Option<&'a str>) -> Self {
        self.lang = lang;
        self
    }
}

/// The NULL handling an `in`/`not in` condition compiled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NullTest {
    /// Only the value test.
    None,
    /// `(<value test> OR col IS NULL)`.
    OrIsNull,
    /// `col IS NULL` (no values).
    IsNull,
    /// `col IS NOT NULL` (no values).
    IsNotNull,
    /// The constant `TRUE`.
    True,
    /// The constant `FALSE`.
    False,
}

/// What a boolean `in`/`not in` compiled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoolTest {
    /// `col IS TRUE`.
    IsTrue,
    /// `col IS NOT TRUE`.
    IsNotTrue,
    /// `TRUE`.
    True,
    /// `FALSE`.
    False,
}

/// An inequality operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Inequality {
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    Le,
    /// `>=`.
    Ge,
}

impl Inequality {
    fn symbol(self) -> &'static str {
        match self {
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Ge => ">=",
        }
    }
}

/// A column of one of a scope's tables: `table` 0 is the scope's own table, `i` the `i`-th join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Col {
    /// Table index in the scope.
    pub table: u16,
    /// Field of that table's model.
    pub field: FieldId,
}

/// A join of a scope: `<kind> comodel AS alias ON (from.field = alias.id)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JoinShape {
    /// `LEFT JOIN` rather than `JOIN`.
    pub left: bool,
    /// The joined table's alias.
    pub alias: Arc<str>,
    /// The model joined.
    pub model: ModelId,
    /// The many2one followed.
    pub from: Col,
}

/// A query on one model: its alias, its joins and its predicate tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeShape {
    /// The scope's model.
    pub model: ModelId,
    /// Alias of the scope's table.
    pub alias: Arc<str>,
    /// Joins, in creation order (table index = position + 1).
    pub joins: Vec<JoinShape>,
    /// The predicate.
    pub tokens: Vec<Token>,
}

/// A relational sub-query (REL §4.14).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubShape {
    /// `[NOT ]EXISTS(SELECT FROM (SELECT co.inverse AS __inverse FROM co … WHERE …) AS __sub
    /// WHERE __inverse = outer.id)`.
    O2m {
        /// `NOT EXISTS`.
        negated: bool,
        /// The table whose id is correlated.
        outer: u16,
        /// The inverse field of the comodel.
        inverse: FieldId,
        /// The comodel query.
        scope: ScopeShape,
    },
    /// `[NOT ]EXISTS (SELECT 1 FROM rel AS alias WHERE alias.c1 = outer.id [AND alias.c2 IN
    /// (SELECT co.id FROM co … WHERE …)])`.
    M2m {
        /// `NOT EXISTS`.
        negated: bool,
        /// The many2many field (of the outer table's model).
        field: Col,
        /// Alias of the relation table.
        alias: Arc<str>,
        /// The comodel query, `None` when it restricts nothing.
        scope: Option<ScopeShape>,
    },
    /// `f IN (SELECT co.id FROM co … [WHERE …])`.
    M2oIn {
        /// The many2one column.
        col: Col,
        /// The comodel query.
        scope: ScopeShape,
    },
    /// `NOT EXISTS (SELECT 1 FROM co AS alias … WHERE alias.id = f [AND …])`.
    M2oNotExists {
        /// The many2one column.
        col: Col,
        /// The comodel query (correlated).
        scope: ScopeShape,
    },
}

/// One element of a predicate's shape. Parameters are consumed in token order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Token {
    /// Physical property map is non-null (a proven-safe partial-index prefilter).
    MapNotNull(Col),
    /// `TRUE` / `FALSE`.
    Const(bool),
    /// `(` of an AND/OR.
    Open,
    /// `)` of an AND/OR or of a guard.
    Close,
    /// ` AND ` between children.
    And,
    /// ` OR ` between children.
    Or,
    /// `(` of a surviving NOT.
    NotOpen,
    /// `) IS NOT TRUE` of a surviving NOT.
    NotClose,
    /// `in` / `not in`; `values` is the array parameter's type when there is one.
    In {
        /// The column.
        col: Col,
        /// `in` (true) or `not in`.
        positive: bool,
        /// Type of the array parameter, `None` when no value is left.
        values: Option<ScalarKind>,
        /// NULL handling.
        null: NullTest,
    },
    /// Boolean `in` / `not in`.
    Bool {
        /// The column.
        col: Col,
        /// What it compiled to.
        test: BoolTest,
    },
    /// JSON "set" / "not set".
    JsonNull {
        /// The column.
        col: Col,
        /// `IS NULL` (true) or `IS NOT NULL`.
        is_null: bool,
    },
    /// The `like` family (one text parameter).
    Like {
        /// The column.
        col: Col,
        /// `ILIKE` rather than `LIKE`.
        insensitive: bool,
        /// A negative operator (`NOT LIKE`).
        negative: bool,
        /// The column is cast to text (non-text types).
        cast: bool,
        /// `OR col IS NULL` (negative operator on a nullable column).
        or_null: bool,
    },
    /// An inequality (one parameter).
    Inequality {
        /// The column.
        col: Col,
        /// The operator.
        op: Inequality,
        /// Parameter type.
        kind: ScalarKind,
        /// `OR col IS NULL` (the falsy value satisfies the comparison).
        or_null: bool,
    },
    /// `(col IS NOT NULL AND <join alias>.id IS NOT NULL AND ` before a joined sub-predicate,
    /// closed by [`Token::Close`].
    Guard {
        /// The many2one column.
        col: Col,
        /// The joined table.
        join: u16,
    },
    /// A relational sub-query.
    Sub(Box<SubShape>),
    /// `col [NOT ]IN (WITH RECURSIVE …)`: a `child_of`/`parent_of` hierarchy (one `int8[]`
    /// parameter, the start ids).
    Hierarchy {
        /// The id or many2one column.
        col: Col,
        /// `IN` (true) or `NOT IN`.
        positive: bool,
        /// The model of the hierarchy.
        model: ModelId,
        /// Its parent many2one.
        parent: FieldId,
        /// `child_of` (true) or `parent_of`.
        descendants: bool,
        /// `child_of` by `parent_path` prefixes (one `text[]` parameter) instead of ids.
        by_path: bool,
        /// `col` may be NULL (a negated test then accepts it).
        nullable: bool,
    },
    /// A hierarchy whose start records are selected in the statement (a
    /// `child_of`/`parent_of` over principal parameters): `col [NOT] IN (<walk from the records
    /// matching the literal ids or a parameter>)`.
    HierarchyFrom {
        /// The id or many2one column.
        col: Col,
        /// `IN` (true) or `NOT IN`.
        positive: bool,
        /// The model of the hierarchy.
        model: ModelId,
        /// Its parent many2one.
        parent: FieldId,
        /// `child_of` (true) or `parent_of`.
        descendants: bool,
        /// `child_of` by `parent_path` ranges under the start records' paths.
        by_path: bool,
        /// `col` may be NULL (a negated test then accepts it).
        nullable: bool,
        /// Literal start ids (one `int8[]` parameter, first).
        literals: bool,
        /// One test per principal parameter, in order.
        params: Vec<ParamTest>,
    },
    /// `in`/`not in` whose members include principal parameters: the literal members
    /// as [`Token::In`] plans them, then one test per parameter, ORed (`in`) or ANDed (`not in`).
    ParamIn {
        /// The column.
        col: Col,
        /// `in` (true) or `not in`.
        positive: bool,
        /// Type of the literal members' array parameter, `None` when there are none.
        literals: Option<ScalarKind>,
        /// NULL handling of the literal members (`False` and falsy values among them).
        null: NullTest,
        /// `col` may be NULL.
        nullable: bool,
        /// The parameter members, in order.
        params: Vec<ParamTest>,
    },
    /// An inequality with "now": `col <op> (now() AT TIME ZONE 'UTC')[::date]` (the
    /// transaction's start time, no parameter).
    NowCompare {
        /// The column.
        col: Col,
        /// The operator.
        op: Inequality,
        /// Date or timestamp.
        now: NowCast,
    },
    /// `col && $n::daterange|tsrange`: `overlaps` on a range field (one range parameter, with the
    /// field's bounds).
    RangeOverlaps {
        /// The range column.
        col: Col,
        /// `daterange` or `tsrange`.
        kind: ScalarKind,
    },
    /// `col @> $n::date|timestamp`: `contains` on a range field (one element parameter).
    RangeContains {
        /// The range column.
        col: Col,
        /// `date` or `timestamp`.
        kind: ScalarKind,
    },
}

/// How one principal-parameter member of an `in` set compares (lowering table).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParamTest {
    /// A scalar that is never NULL (`user.id`, `company_id`): `col = $n`.
    Scalar(ScalarKind),
    /// A collection (`company_ids`, `grant_ids`): `col = ANY($n::kind[])`.
    Array(ScalarKind),
    /// A scalar user path.
    PathScalar(UserPathShape),
    /// A user path crossing an x2many field: `col IN (SELECT …)`.
    PathSet(UserPathShape),
    /// The transaction's start: `col = (now() AT TIME ZONE 'UTC')[::date]`.
    Now(NowCast),
}

/// One relational hop of a user path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathHop {
    /// The model the hop leaves.
    pub model: ModelId,
    /// Its relational field followed.
    pub field: FieldId,
    /// The model reached.
    pub comodel: ModelId,
}

/// A user path planned against the registry: `SELECT <value> FROM res_users <hops> WHERE
/// res_users.id = $uid`, one parameter (the subject).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserPathShape {
    /// `res.users`.
    pub users: ModelId,
    /// The hops, in order.
    pub hops: Vec<PathHop>,
    /// The selected column of the last model reached (its `id` or a many2one).
    pub value: FieldId,
}

/// The literal-free description of a predicate's text: equal keys render equal text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShapeKey {
    /// Bound prepared-company context slot, absent for ordinary expressions.
    pub company_slot: Option<usize>,
    company_expressions: Vec<(ModelId, Arc<str>, FieldId, String)>,
    company_targets: Vec<ModelId>,
    /// The model (field ids are relative to the scopes' models).
    pub model: ModelId,
    /// Accent folding in effect.
    pub unaccent: Unaccent,
    /// Language code for translated fields.
    pub lang: Option<Arc<str>>,
    /// The main query.
    pub root: ScopeShape,
}

impl ShapeKey {
    /// Whether the predicate reads rows of `model`'s table other than the row it is evaluated on:
    /// a join, a relational sub-select, a hierarchy walk or a user path through that table. A
    /// `create` cannot evaluate such a predicate in its own `INSERT … RETURNING` (the statement's
    /// sub-queries do not see the rows it inserts).
    pub fn selects_from(&self, model: ModelId) -> bool {
        self.company_targets.contains(&model) || scope_selects(&self.root, model, true)
    }
}

fn scope_selects(scope: &ScopeShape, model: ModelId, root: bool) -> bool {
    (!root && scope.model == model)
        || scope.joins.iter().any(|j| j.model == model)
        || scope.tokens.iter().any(|t| token_selects(t, model))
}

fn token_selects(token: &Token, model: ModelId) -> bool {
    match token {
        Token::Sub(sub) => match &**sub {
            SubShape::O2m { scope, .. }
            | SubShape::M2oIn { scope, .. }
            | SubShape::M2oNotExists { scope, .. } => scope_selects(scope, model, false),
            SubShape::M2m { scope, .. } => scope
                .as_ref()
                .is_some_and(|s| scope_selects(s, model, false)),
        },
        Token::Hierarchy { model: tree, .. } => *tree == model,
        Token::HierarchyFrom {
            model: tree,
            params,
            ..
        } => *tree == model || params.iter().any(|p| param_selects(p, model)),
        Token::ParamIn { params, .. } => params.iter().any(|p| param_selects(p, model)),
        // own-row columns and fixed texts: nothing is selected from another table
        Token::MapNotNull(_)
        | Token::Const(_)
        | Token::Open
        | Token::Close
        | Token::And
        | Token::Or
        | Token::NotOpen
        | Token::NotClose
        | Token::In { .. }
        | Token::Bool { .. }
        | Token::JsonNull { .. }
        | Token::Like { .. }
        | Token::Inequality { .. }
        | Token::Guard { .. }
        | Token::NowCompare { .. }
        | Token::RangeOverlaps { .. }
        | Token::RangeContains { .. } => false,
    }
}

fn param_selects(test: &ParamTest, model: ModelId) -> bool {
    match test {
        ParamTest::PathScalar(path) | ParamTest::PathSet(path) => {
            path.users == model || path.hops.iter().any(|h| h.comodel == model)
        }
        _ => false,
    }
}

/// The value-dependent part of a compilation: the shape, the parameters and the fields read.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The shape, the memo key of the text.
    pub key: ShapeKey,
    /// Parameters for `$1…$n`, in order.
    pub params: Vec<Param>,
    /// Fields the predicate reads, with their model (their pending writes are flushed first).
    pub fields: Vec<(ModelId, FieldId)>,
    /// `Some(b)` when the domain is the constant `b` (no query is needed for `FALSE`).
    pub constant: Option<bool>,
}

/// The rendered text of a shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    /// The predicate; `TRUE` or `FALSE` for constant domains.
    pub sql: String,
    /// Joins of the main table, `(alias, " JOIN … ON (…)")`, in order.
    pub joins: Vec<(String, String)>,
}

/// A compiled predicate (plan and text).
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// The predicate; `TRUE` or `FALSE` for constant domains.
    pub sql: String,
    /// Joins of the main table, in order.
    pub joins: Vec<(String, String)>,
    /// Parameters for `$1…$n`, in order.
    pub params: Vec<Param>,
    /// Fields the predicate reads, with their model.
    pub fields: Vec<(ModelId, FieldId)>,
    /// `Some(b)` when the domain is the constant `b`.
    pub constant: Option<bool>,
}

pub fn make_alias(from: &str, link: &str) -> Arc<str> {
    Arc::from(shorten_identifier(&format!("{from}__{link}")))
}

/// Plan a FULL-optimized domain on `ctx.model`'s table.
pub fn plan_where(domain: &Domain, ctx: &WhereContext<'_>) -> Result<Plan> {
    plan_where_inner(domain, ctx, None, None)
}

/// Discover physical dependencies without rendering or inventing property defaults.
/// # Errors
/// As [`plan_where`], except that property preparation is not required for discovery.
pub fn where_fields(domain: &Domain, ctx: &WhereContext<'_>) -> Result<Vec<(ModelId, FieldId)>> {
    Ok(plan_where_inner(domain, ctx, Some(1), None)?.fields)
}

/// Plan with prepared values bound by the caller at `slot`; literal parameters start after it.
/// # Errors
/// As [`plan_where`], plus missing prepared fallbacks.
pub fn plan_where_with_company(
    domain: &Domain,
    ctx: &WhereContext<'_>,
    values: &crate::company::CompanyValues,
    slot: usize,
) -> Result<Plan> {
    if slot == 0 {
        return Err(Error::programming(
            "prepared company parameter slot must be positive",
        ));
    }
    let plan = plan_where_inner(domain, ctx, Some(slot), Some(values))?;
    values.validate(ctx.registry, &plan.fields)?;
    Ok(plan)
}

fn plan_where_inner(
    domain: &Domain,
    ctx: &WhereContext<'_>,
    company_slot: Option<usize>,
    company: Option<&crate::company::CompanyValues>,
) -> Result<Plan> {
    crate::validate_language(ctx.lang)?;
    let constant = match domain.kind() {
        DomainKind::True => Some(true),
        DomainKind::False => Some(false),
        _ => None,
    };
    let mut planner = Planner {
        ctx,
        company_slot,
        company,
        params: Vec::new(),
        fields: Vec::new(),
        external: domain.is_external(),
    };
    let mut scope = ScopeBuild::new(ctx.model, Arc::from(ctx.model.table.as_str()));
    planner.node(domain, &mut scope, 0)?;
    for (model, field) in &planner.fields {
        let model = ctx.registry.model_by_id(*model);
        let field = model.field_by_id(*field);
        if field.company_dependent && company_slot.is_none() {
            return Err(crate::company::missing(model, field));
        }
    }
    let mut company_targets = Vec::new();
    for &(model, field) in &planner.fields {
        let field = ctx.registry.model_by_id(model).field_by_id(field);
        if field.company_dependent
            && let FieldType::Many2one { comodel, .. } = &field.ty
        {
            let target = ctx
                .registry
                .model(comodel)
                .ok_or_else(|| Error::programming("unknown company-dependent comodel"))?;
            if !company_targets.contains(&target.id) {
                company_targets.push(target.id);
            }
        }
    }
    for target in &company_targets {
        let spec = ctx.registry.model_by_id(*target);
        if let Some(id) = spec.field("id")
            && !planner.fields.contains(&(*target, id.id))
        {
            planner.fields.push((*target, id.id));
        }
    }
    let root = scope.finish();
    let mut company_expressions = Vec::new();
    if let Some(slot) = company_slot {
        collect_company_expressions(
            &root,
            ctx.registry,
            &planner.fields,
            slot,
            &mut company_expressions,
        )?;
    }
    Ok(Plan {
        key: ShapeKey {
            company_slot,
            company_expressions,
            company_targets,
            model: ctx.model.id,
            unaccent: ctx.unaccent,
            lang: ctx.lang.map(Arc::from),
            root,
        },
        params: planner.params,
        fields: planner.fields,
        constant,
    })
}

fn collect_company_expressions(
    scope: &ScopeShape,
    registry: &Registry,
    fields: &[(ModelId, FieldId)],
    slot: usize,
    out: &mut Vec<(ModelId, Arc<str>, FieldId, String)>,
) -> Result<()> {
    for (model, alias) in std::iter::once((scope.model, &scope.alias))
        .chain(scope.joins.iter().map(|j| (j.model, &j.alias)))
    {
        let spec = registry.model_by_id(model);
        for &(_, field) in fields.iter().filter(|(m, _)| *m == model) {
            let field = spec.field_by_id(field);
            if field.company_dependent {
                out.push((
                    spec.id,
                    Arc::clone(alias),
                    field.id,
                    crate::company::expression(registry, spec, field, alias, slot)?,
                ));
            }
        }
    }
    for token in &scope.tokens {
        if let Token::ParamIn { params, .. } | Token::HierarchyFrom { params, .. } = token {
            for param in params {
                if let ParamTest::PathScalar(path) | ParamTest::PathSet(path) = param {
                    for (i, hop) in path.hops.iter().enumerate() {
                        let model = registry.model_by_id(hop.model);
                        let field = model.field_by_id(hop.field);
                        let alias: Arc<str> = Arc::from(format!("__up{i}"));
                        if field.company_dependent {
                            out.push((
                                model.id,
                                Arc::clone(&alias),
                                field.id,
                                crate::company::expression(registry, model, field, &alias, slot)?,
                            ));
                        }
                    }
                    let last = path.hops.last().map_or(path.users, |hop| hop.comodel);
                    let model = registry.model_by_id(last);
                    let field = model.field_by_id(path.value);
                    let alias: Arc<str> = Arc::from(format!("__up{}", path.hops.len()));
                    if field.company_dependent {
                        out.push((
                            model.id,
                            Arc::clone(&alias),
                            field.id,
                            crate::company::expression(registry, model, field, &alias, slot)?,
                        ));
                    }
                }
            }
        }
        if let Token::Sub(sub) = token {
            match &**sub {
                SubShape::O2m { scope, .. }
                | SubShape::M2oIn { scope, .. }
                | SubShape::M2oNotExists { scope, .. } => {
                    collect_company_expressions(scope, registry, fields, slot, out)?
                }
                SubShape::M2m {
                    scope: Some(scope), ..
                } => collect_company_expressions(scope, registry, fields, slot, out)?,
                SubShape::M2m { scope: None, .. } => {}
            }
        }
    }
    Ok(())
}

/// Render a shape. Placeholders are numbered from 1 in token order, so the text pairs with the
/// parameters of any plan that has this key.
pub fn render_where(key: &ShapeKey, registry: &Registry) -> Rendered {
    let mut r = Renderer {
        registry,
        unaccent: key.unaccent,
        lang: key.lang.clone(),
        company_expressions: &key.company_expressions,
        n: 0,
    };
    let joins = r.joins(&key.root);
    let sql = r.tokens(&key.root);
    Rendered { sql, joins }
}

/// Render a shape whose placeholders start after `offset` (`$offset+1`…): the predicate of a
/// sub-query embedded in a statement that already binds `offset` parameters.
pub fn render_where_at(key: &ShapeKey, registry: &Registry, offset: usize) -> Rendered {
    let mut r = Renderer {
        registry,
        unaccent: key.unaccent,
        lang: key.lang.clone(),
        company_expressions: &key.company_expressions,
        n: offset,
    };
    let joins = r.joins(&key.root);
    let sql = r.tokens(&key.root);
    Rendered { sql, joins }
}

/// Plan and render in one go (no memo).
pub fn compile_where(domain: &Domain, ctx: &WhereContext<'_>) -> Result<Filter> {
    let plan = plan_where(domain, ctx)?;
    let rendered = render_where(&plan.key, ctx.registry);
    Ok(Filter {
        sql: rendered.sql,
        joins: rendered.joins,
        params: plan.params,
        fields: plan.fields,
        constant: plan.constant,
    })
}

/// Compile a predicate with its prepared context as the first bound parameter.
/// # Errors
/// As [`plan_where_with_company`].
pub fn compile_where_with_company(
    domain: &Domain,
    ctx: &WhereContext<'_>,
    values: &crate::company::CompanyValues,
) -> Result<Filter> {
    let fields = where_fields(domain, ctx)?;
    if !fields.iter().any(|(model, field)| {
        ctx.registry
            .model_by_id(*model)
            .field_by_id(*field)
            .company_dependent
    }) {
        return compile_where(domain, ctx);
    }
    let plan = plan_where_with_company(domain, ctx, values, 1)?;
    let rendered = render_where_at(&plan.key, ctx.registry, 1);
    let mut params = vec![values.param()?];
    params.extend(plan.params);
    Ok(Filter {
        sql: rendered.sql,
        joins: rendered.joins,
        params,
        fields: plan.fields,
        constant: plan.constant,
    })
}

/// The SQL text of a field's column on `model`'s table: `"table"."field"`.
pub fn column(model: &ModelSpec, field: &str) -> String {
    format!("{}.{}", quote(&model.table), quote(field))
}

pub fn can_be_null(field: &FieldSpec) -> bool {
    if field.translate {
        return true;
    }
    field.company_dependent
        || !(matches!(field.ty, FieldType::Id) || (field.required && field.has_column()))
}

fn is_text(ty: &FieldType) -> bool {
    matches!(
        ty,
        FieldType::Char { .. } | FieldType::Text | FieldType::Html | FieldType::Reference { .. }
    )
}

// ----- rendering --------------------------------------------------------------------------------

struct Renderer<'a> {
    registry: &'a Registry,
    unaccent: Unaccent,
    lang: Option<Arc<str>>,
    n: usize,
    company_expressions: &'a [(ModelId, Arc<str>, FieldId, String)],
}

impl Renderer<'_> {
    fn next(&mut self, cast: &str) -> String {
        self.n += 1;
        format!("${}::{cast}", self.n)
    }

    fn table_alias<'s>(&self, scope: &'s ScopeShape, table: u16) -> &'s str {
        match table {
            0 => &scope.alias,
            i => scope
                .joins
                .get(usize::from(i) - 1)
                .map_or(&*scope.alias, |j| &*j.alias),
        }
    }

    fn table_model(&self, scope: &ScopeShape, table: u16) -> ModelId {
        match table {
            0 => scope.model,
            i => scope
                .joins
                .get(usize::from(i) - 1)
                .map_or(scope.model, |j| j.model),
        }
    }

    fn col(&self, scope: &ScopeShape, col: Col) -> String {
        let model = self
            .registry
            .model_by_id(self.table_model(scope, col.table));
        let field = model.field_by_id(col.field);
        self.value_at(model, field, self.table_alias(scope, col.table))
    }

    fn value_at(&self, model: &ModelSpec, field: &FieldSpec, alias: &str) -> String {
        let table_col = format!("{}.{}", quote(alias), quote(field.name));
        if field.company_dependent {
            // Keys carry prevalidated expressions. A caller mutating a key's public scope
            // after planning must never fall back to comparing its physical JSON map.
            return match self.company_expressions.iter().find(
                |(prepared_model, prepared_alias, fid, _)| {
                    *prepared_model == model.id
                        && prepared_alias.as_ref() == alias
                        && *fid == field.id
                },
            ) {
                Some((_, _, _, sql)) => sql.clone(),
                None => "CAST('company-dependent SQL requires prepared company values' AS int8)"
                    .to_owned(),
            };
        }
        if field.translate {
            let lang_code = self
                .lang
                .as_deref()
                .filter(|l| !l.is_empty())
                .unwrap_or("en_US");
            if lang_code == "en_US" {
                format!("{table_col}->>'en_US'")
            } else {
                format!(
                    "COALESCE({table_col}->>{}, {table_col}->>'en_US')",
                    crate::literal(lang_code)
                )
            }
        } else {
            table_col
        }
    }

    fn id_of(&self, scope: &ScopeShape, table: u16) -> String {
        format!("{}.{}", quote(self.table_alias(scope, table)), quote("id"))
    }

    fn joins(&self, scope: &ScopeShape) -> Vec<(String, String)> {
        scope
            .joins
            .iter()
            .map(|j| {
                let model = self.registry.model_by_id(j.model);
                let kind = if j.left { "LEFT JOIN" } else { "JOIN" };
                (
                    j.alias.to_string(),
                    format!(
                        " {kind} {} AS {} ON ({} = {}.{})",
                        quote(&model.table),
                        quote(&j.alias),
                        self.col(scope, j.from),
                        quote(&j.alias),
                        quote("id")
                    ),
                )
            })
            .collect()
    }

    /// `FROM <table> AS <alias><joins>` of a sub-query.
    fn from(&self, scope: &ScopeShape) -> String {
        let model = self.registry.model_by_id(scope.model);
        let mut out = if *scope.alias == *model.table {
            quote(&model.table)
        } else {
            format!("{} AS {}", quote(&model.table), quote(&scope.alias))
        };
        for (_, join) in self.joins(scope) {
            out.push_str(&join);
        }
        out
    }

    /// The predicate of a sub-query, or `None` when it is `TRUE`.
    fn where_of(&mut self, scope: &ScopeShape) -> Option<String> {
        if scope.tokens.is_empty() || scope.tokens == [Token::Const(true)] {
            return None;
        }
        Some(self.tokens(scope))
    }

    fn tokens(&mut self, scope: &ScopeShape) -> String {
        let mut out = String::new();
        for token in &scope.tokens {
            self.token(scope, token, &mut out);
        }
        out
    }

    fn token(&mut self, scope: &ScopeShape, token: &Token, out: &mut String) {
        match token {
            Token::MapNotNull(col) => {
                let model = self
                    .registry
                    .model_by_id(self.table_model(scope, col.table));
                out.push_str(&format!(
                    "{}.{} IS NOT NULL",
                    quote(self.table_alias(scope, col.table)),
                    quote(model.field_by_id(col.field).name)
                ));
            }
            Token::Const(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
            Token::Open | Token::NotOpen => out.push('('),
            Token::Close => out.push(')'),
            Token::NotClose => out.push_str(") IS NOT TRUE"),
            Token::And => out.push_str(" AND "),
            Token::Or => out.push_str(" OR "),
            Token::In {
                col,
                positive,
                values,
                null,
            } => {
                let c = self.col(scope, *col);
                let test = values.map(|kind| {
                    let p = self.next(&format!("{}[]", kind.sql()));
                    if *positive {
                        format!("{c} = ANY({p})")
                    } else {
                        format!("{c} <> ALL({p})")
                    }
                });
                out.push_str(&match (null, test) {
                    (NullTest::OrIsNull, Some(t)) => format!("({t} OR {c} IS NULL)"),
                    (NullTest::None, Some(t)) => t,
                    (NullTest::IsNull, _) => format!("{c} IS NULL"),
                    (NullTest::IsNotNull, _) => format!("{c} IS NOT NULL"),
                    (NullTest::True, _) => "TRUE".to_owned(),
                    (NullTest::False | NullTest::None | NullTest::OrIsNull, _) => {
                        "FALSE".to_owned()
                    }
                });
            }
            Token::Bool { col, test } => {
                let c = self.col(scope, *col);
                out.push_str(&match test {
                    BoolTest::IsTrue => format!("{c} IS TRUE"),
                    BoolTest::IsNotTrue => format!("{c} IS NOT TRUE"),
                    BoolTest::True => "TRUE".to_owned(),
                    BoolTest::False => "FALSE".to_owned(),
                });
            }
            Token::JsonNull { col, is_null } => {
                let not = if *is_null { "" } else { "NOT " };
                out.push_str(&format!("{} IS {not}NULL", self.col(scope, *col)));
            }
            Token::Like {
                col,
                insensitive,
                negative,
                cast,
                or_null,
            } => {
                let c = self.col(scope, *col);
                let mut left = if *cast {
                    format!("{c}::text")
                } else {
                    c.clone()
                };
                let mut right = self.next("text");
                if *insensitive {
                    left = self.unaccent.wrap(&left);
                    right = self.unaccent.wrap(&right);
                }
                let keyword = if *insensitive { "ILIKE" } else { "LIKE" };
                let not = if *negative { "NOT " } else { "" };
                let sql = format!("{left} {not}{keyword} {right}");
                out.push_str(&if *or_null {
                    format!("({sql} OR {c} IS NULL)")
                } else {
                    sql
                });
            }
            Token::Inequality {
                col,
                op,
                kind,
                or_null,
            } => {
                let c = self.col(scope, *col);
                let sql = format!("{c} {} {}", op.symbol(), self.next(kind.sql()));
                out.push_str(&if *or_null {
                    format!("({sql} OR {c} IS NULL)")
                } else {
                    sql
                });
            }
            Token::Guard { col, join } => {
                out.push_str(&format!(
                    "({} IS NOT NULL AND {} IS NOT NULL AND ",
                    self.col(scope, *col),
                    self.id_of(scope, *join)
                ));
            }
            Token::Sub(sub) => {
                let text = self.sub(scope, sub);
                out.push_str(&text);
            }
            Token::Hierarchy {
                col,
                positive,
                model,
                parent,
                descendants,
                by_path,
                nullable,
            } => {
                let c = self.col(scope, *col);
                let spec = self.registry.model_by_id(*model);
                let parent = spec.field_by_id(*parent).name;
                let p = self.next(if *by_path { "text[]" } else { "int8[]" });
                let sub = if *by_path {
                    crate::relational::subtree_by_paths(&spec.table, &p)
                } else if *descendants {
                    crate::relational::descendants_of(&spec.table, parent, &p)
                } else {
                    crate::relational::ancestors_of(&spec.table, parent, &p)
                };
                out.push_str(&match (*positive, *nullable) {
                    (true, _) => format!("{c} IN ({sub})"),
                    (false, true) => format!("({c} IS NULL OR {c} NOT IN ({sub}))"),
                    (false, false) => format!("{c} NOT IN ({sub})"),
                });
            }
            Token::HierarchyFrom {
                col,
                positive,
                model,
                parent,
                descendants,
                by_path,
                nullable,
                literals,
                params,
            } => {
                let c = self.col(scope, *col);
                let spec = self.registry.model_by_id(*model);
                let parent = spec.field_by_id(*parent).name;
                let start = self.hierarchy_start(*literals, params);
                let sub = if *by_path {
                    crate::relational::subtree_from(&spec.table, &start)
                } else if *descendants {
                    crate::relational::descendants_from(&spec.table, parent, &start)
                } else {
                    crate::relational::ancestors_from(&spec.table, parent, &start)
                };
                out.push_str(&match (*positive, *nullable) {
                    (true, _) => format!("{c} IN ({sub})"),
                    (false, true) => format!("({c} IS NULL OR {c} NOT IN ({sub}))"),
                    (false, false) => format!("{c} NOT IN ({sub})"),
                });
            }
            Token::ParamIn {
                col,
                positive,
                literals,
                null,
                nullable,
                params,
            } => {
                let text =
                    self.param_in(scope, *col, *positive, *literals, *null, *nullable, params);
                out.push_str(&text);
            }
            Token::NowCompare { col, op, now } => {
                let c = self.col(scope, *col);
                out.push_str(&format!("{c} {} {}", op.symbol(), now_sql(*now)));
            }
            Token::RangeOverlaps { col, kind } => {
                let c = self.col(scope, *col);
                out.push_str(&format!("{c} && {}", self.next(kind.sql())));
            }
            Token::RangeContains { col, kind } => {
                let c = self.col(scope, *col);
                out.push_str(&format!("{c} @> {}", self.next(kind.sql())));
            }
        }
    }

    /// The text of a [`Token::ParamIn`]: the literal part (when it tests anything), then one
    /// test per parameter, ORed for `in` and ANDed for `not in`.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields of one token, destructured by the caller"
    )]
    fn param_in(
        &mut self,
        scope: &ScopeShape,
        col: Col,
        positive: bool,
        literals: Option<ScalarKind>,
        null: NullTest,
        nullable: bool,
        params: &[ParamTest],
    ) -> String {
        let c = self.col(scope, col);
        let mut parts: Vec<String> = Vec::with_capacity(params.len() + 1);
        let literal = literals.map(|kind| {
            let p = self.next(&format!("{}[]", kind.sql()));
            if positive {
                format!("{c} = ANY({p})")
            } else {
                format!("{c} <> ALL({p})")
            }
        });
        // the literal members alone: what `Token::In` would render, minus its neutral constants
        match (null, literal) {
            (NullTest::OrIsNull, Some(t)) => parts.push(format!("({t} OR {c} IS NULL)")),
            (NullTest::None, Some(t)) => parts.push(t),
            (NullTest::IsNull, _) => parts.push(format!("{c} IS NULL")),
            (NullTest::IsNotNull, _) => parts.push(format!("{c} IS NOT NULL")),
            (NullTest::False, _) if !positive => parts.push("FALSE".to_owned()),
            (NullTest::True, _) if positive => parts.push("TRUE".to_owned()),
            // `in` with no literal member, `not in` with none: neutral for the OR/AND below
            (NullTest::False | NullTest::True | NullTest::None | NullTest::OrIsNull, _) => {}
        }
        for test in params {
            parts.push(self.param_test(&c, positive, nullable, test));
        }
        let joiner = if positive { " OR " } else { " AND " };
        match parts.len() {
            0 => (if positive { "FALSE" } else { "TRUE" }).to_owned(),
            1 => parts.remove(0),
            _ => format!("({})", parts.join(joiner)),
        }
    }

    /// The condition on `"__s"."id"` selecting the start records of a [`Token::HierarchyFrom`]:
    /// the literal ids, then each parameter, ORed. An id is never NULL, so a scalar user path
    /// compares with `=` alone (a NULL path selects no start record).
    fn hierarchy_start(&mut self, literals: bool, params: &[ParamTest]) -> String {
        let s = "\"__s\".\"id\"";
        let mut parts: Vec<String> = Vec::with_capacity(params.len() + 1);
        if literals {
            let p = self.next("int8[]");
            parts.push(format!("{s} = ANY({p})"));
        }
        for test in params {
            parts.push(match test {
                ParamTest::PathScalar(path) => {
                    let sub = self.user_path(path);
                    format!("{s} = ({sub})")
                }
                other => self.param_test(s, true, false, other),
            });
        }
        match parts.len() {
            0 => "FALSE".to_owned(),
            1 => parts.remove(0),
            _ => format!("({})", parts.join(" OR ")),
        }
    }

    fn param_test(&mut self, c: &str, positive: bool, nullable: bool, test: &ParamTest) -> String {
        let or_null = |t: String| {
            if nullable {
                format!("({t} OR {c} IS NULL)")
            } else {
                t
            }
        };
        match test {
            ParamTest::Scalar(kind) => {
                let p = self.next(kind.sql());
                if positive {
                    format!("{c} = {p}")
                } else {
                    or_null(format!("{c} <> {p}"))
                }
            }
            ParamTest::Array(kind) => {
                let p = self.next(&format!("{}[]", kind.sql()));
                if positive {
                    format!("{c} = ANY({p})")
                } else {
                    or_null(format!("{c} <> ALL({p})"))
                }
            }
            ParamTest::PathScalar(path) => {
                let nullable = self.path_nullable(path);
                let sub = self.user_path(path);
                if positive && !nullable {
                    format!("{c} = ({sub})")
                } else if positive {
                    format!("({c} = ({sub}) OR ({c} IS NULL AND ({sub}) IS NULL))")
                } else {
                    format!("{c} IS DISTINCT FROM ({sub})")
                }
            }
            ParamTest::PathSet(path) => {
                let sub = self.user_path(path);
                if positive {
                    format!("{c} IN ({sub})")
                } else {
                    or_null(format!("{c} NOT IN ({sub})"))
                }
            }
            ParamTest::Now(now) => {
                let now = now_sql(*now);
                if positive {
                    format!("{c} = {now}")
                } else {
                    or_null(format!("{c} <> {now}"))
                }
            }
        }
    }

    /// `SELECT <value> FROM "res_users" AS "__up0" <joins> WHERE "__up0"."id" = $n::int8`.
    /// Whether the path's value can be NULL: some hop is not a required many2one, or the value
    /// column is nullable (a hop through an x2many may reach no row at all).
    fn path_nullable(&self, path: &UserPathShape) -> bool {
        let required_m2o = |model: ModelId, field: FieldId| {
            let f = self.registry.model_by_id(model).field_by_id(field);
            matches!(f.ty, FieldType::Many2one { .. }) && !f.company_dependent && !can_be_null(f)
        };
        if path
            .hops
            .iter()
            .any(|hop| !required_m2o(hop.model, hop.field))
        {
            return true;
        }
        let last = path.hops.last().map_or(path.users, |h| h.comodel);
        let value = self.registry.model_by_id(last).field_by_id(path.value);
        value.company_dependent || can_be_null(value)
    }

    fn user_path(&mut self, path: &UserPathShape) -> String {
        let alias = |i: usize| quote(&format!("__up{i}"));
        let users = self.registry.model_by_id(path.users);
        let mut from = format!("{} AS {}", quote(&users.table), alias(0));
        for (i, hop) in path.hops.iter().enumerate() {
            let source = self.registry.model_by_id(hop.model);
            let field = source.field_by_id(hop.field);
            let comodel = self.registry.model_by_id(hop.comodel);
            let (here, next) = (alias(i), alias(i + 1));
            match &field.ty {
                FieldType::One2many { inverse, .. } => from.push_str(&format!(
                    " JOIN {} AS {next} ON ({next}.{} = {here}.{})",
                    quote(&comodel.table),
                    quote(inverse),
                    quote("id")
                )),
                FieldType::Many2many {
                    relation: Some(rel),
                    ..
                } => {
                    let link = quote(&format!("__upr{i}"));
                    from.push_str(&format!(
                        " JOIN {} AS {link} ON ({link}.{} = {here}.{}) JOIN {} AS {next} ON \
                         ({next}.{} = {link}.{})",
                        quote(&rel.table),
                        quote(&rel.column1),
                        quote("id"),
                        quote(&comodel.table),
                        quote("id"),
                        quote(&rel.column2)
                    ));
                }
                // many2one (planning accepts only stored relational fields)
                _ => {
                    let source = self.value_at(source, field, &format!("__up{i}"));
                    from.push_str(&format!(
                        " JOIN {} AS {next} ON ({source} = {next}.{})",
                        quote(&comodel.table),
                        quote("id")
                    ));
                }
            }
        }
        let last = path.hops.last().map_or(path.users, |h| h.comodel);
        let model = self.registry.model_by_id(last);
        let value = self.value_at(
            model,
            model.field_by_id(path.value),
            &format!("__up{}", path.hops.len()),
        );
        let uid = self.next("int8");
        format!(
            "SELECT {value} FROM {from} WHERE {}.{} = {uid}",
            alias(0),
            quote("id")
        )
    }

    fn sub(&mut self, outer: &ScopeShape, sub: &SubShape) -> String {
        match sub {
            SubShape::O2m {
                negated,
                outer: table,
                inverse,
                scope,
            } => {
                let inverse_col = self.col(
                    scope,
                    Col {
                        table: 0,
                        field: *inverse,
                    },
                );
                let from = self.from(scope);
                let mut inner = format!("SELECT {inverse_col} AS \"__inverse\" FROM {from}");
                if let Some(predicate) = self.where_of(scope) {
                    inner.push_str(" WHERE ");
                    inner.push_str(&predicate);
                }
                format!(
                    "{}EXISTS(SELECT FROM ({inner}) AS \"__sub\" WHERE \"__inverse\" = {})",
                    if *negated { "NOT " } else { "" },
                    self.id_of(outer, *table)
                )
            }
            SubShape::M2m {
                negated,
                field,
                alias,
                scope,
            } => {
                let model = self
                    .registry
                    .model_by_id(self.table_model(outer, field.table));
                let (table, c1, c2) = match &model.field_by_id(field.field).ty {
                    FieldType::Many2many {
                        relation: Some(rel),
                        ..
                    } => (
                        rel.table.as_str(),
                        rel.column1.as_str(),
                        rel.column2.as_str(),
                    ),
                    // planning refuses non-stored many2many fields
                    _ => ("", "", ""),
                };
                let mut out = format!(
                    "{}EXISTS (SELECT 1 FROM {} AS {a} WHERE {a}.{} = {}",
                    if *negated { "NOT " } else { "" },
                    quote(table),
                    quote(c1),
                    self.id_of(outer, field.table),
                    a = quote(alias),
                );
                let id_field = self
                    .registry
                    .model_by_id(scope.as_ref().map_or(outer.model, |s| s.model))
                    .field_id("id");
                let ids_only = scope.as_ref().is_some_and(|s| {
                    s.joins.is_empty()
                        && matches!(
                            s.tokens.as_slice(),
                            [Token::In {
                                col: Col { table: 0, field },
                                positive: true,
                                values: Some(ScalarKind::Int8),
                                null: NullTest::None,
                            }] if Some(*field) == id_field
                        )
                });
                if ids_only {
                    let p = self.next("int8[]");
                    out.push_str(&format!(" AND {}.{} = ANY({p})", quote(alias), quote(c2)));
                } else if let Some(scope) = scope {
                    let from = self.from(scope);
                    let id = self.id_of(scope, 0);
                    out.push_str(&format!(
                        " AND {}.{} IN (SELECT {id} FROM {from}",
                        quote(alias),
                        quote(c2)
                    ));
                    if let Some(predicate) = self.where_of(scope) {
                        out.push_str(" WHERE ");
                        out.push_str(&predicate);
                    }
                    out.push(')');
                }
                out.push(')');
                out
            }
            SubShape::M2oIn { col, scope } => {
                let c = self.col(outer, *col);
                let from = self.from(scope);
                let id = self.id_of(scope, 0);
                let mut out = format!("{c} IN (SELECT {id} FROM {from}");
                if let Some(predicate) = self.where_of(scope) {
                    out.push_str(" WHERE ");
                    out.push_str(&predicate);
                }
                out.push(')');
                out
            }
            SubShape::M2oNotExists { col, scope } => {
                let c = self.col(outer, *col);
                let from = self.from(scope);
                let id = self.id_of(scope, 0);
                let mut out = format!("NOT EXISTS (SELECT 1 FROM {from} WHERE {id} = {c}");
                if let Some(predicate) = self.where_of(scope) {
                    out.push_str(" AND ");
                    out.push_str(&predicate);
                }
                out.push(')');
                out
            }
        }
    }
}

// ----- planning ---------------------------------------------------------------------------------

/// A table of a scope being planned.
#[derive(Debug, Clone)]
struct TableRef {
    model: ModelId,
    alias: Arc<str>,
    /// The scope's table, or reached through `JOIN`s only.
    inner: bool,
}

struct ScopeBuild {
    model: ModelId,
    alias: Arc<str>,
    tables: Vec<TableRef>,
    joins: Vec<JoinShape>,
    tokens: Vec<Token>,
}

impl ScopeBuild {
    fn new(model: &ModelSpec, alias: Arc<str>) -> Self {
        Self {
            model: model.id,
            alias: Arc::clone(&alias),
            tables: vec![TableRef {
                model: model.id,
                alias,
                inner: true,
            }],
            joins: Vec::new(),
            tokens: Vec::new(),
        }
    }

    fn finish(self) -> ScopeShape {
        ScopeShape {
            model: self.model,
            alias: self.alias,
            joins: self.joins,
            tokens: self.tokens,
        }
    }
}

struct Planner<'a> {
    ctx: &'a WhereContext<'a>,
    company_slot: Option<usize>,
    company: Option<&'a crate::company::CompanyValues>,
    params: Vec<Param>,
    fields: Vec<(ModelId, FieldId)>,
    external: bool,
}

impl<'a> Planner<'a> {
    /// An error caused by the domain's content: `Validation` for external input (the client's
    /// fault), `Programming` for domains built in Rust.
    fn input_error(&self, message: String) -> Error {
        if self.external {
            Error::validation(message)
        } else {
            Error::programming(message)
        }
    }

    fn not_supported(&self, what: String, slice: &str) -> Error {
        self.input_error(format!("{what} is not supported until {slice}"))
    }

    fn invalid_value(
        &self,
        model: &ModelSpec,
        field: &FieldSpec,
        value: &Value,
        condition: &Condition,
    ) -> Error {
        self.input_error(format!(
            "Invalid value for {}.{}: {value} in condition {condition:?}",
            model.name, field.name
        ))
    }

    fn model(&self, id: ModelId) -> &'a ModelSpec {
        self.ctx.registry.model_by_id(id)
    }

    fn read(&mut self, model: ModelId, field: &FieldSpec) {
        let entry = (model, field.id);
        if !matches!(field.ty, FieldType::Id) && !self.fields.contains(&entry) {
            self.fields.push(entry);
        }
    }

    fn node(&mut self, domain: &Domain, scope: &mut ScopeBuild, table: u16) -> Result<()> {
        match domain.kind() {
            DomainKind::True => scope.tokens.push(Token::Const(true)),
            DomainKind::False => scope.tokens.push(Token::Const(false)),
            DomainKind::Not(child) => {
                // A NOT that survives optimization: NULL results count as "not matching".
                scope.tokens.push(Token::NotOpen);
                self.node(child, scope, table)?;
                scope.tokens.push(Token::NotClose);
            }
            DomainKind::And(children) | DomainKind::Or(children) => {
                let sep = if matches!(domain.kind(), DomainKind::And(_)) {
                    Token::And
                } else {
                    Token::Or
                };
                scope.tokens.push(Token::Open);
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        scope.tokens.push(sep.clone());
                    }
                    self.node(child, scope, table)?;
                }
                scope.tokens.push(Token::Close);
            }
            DomainKind::Condition(condition) => self.condition(condition, scope, table)?,
            DomainKind::Custom(custom) => {
                return Err(self.not_supported(
                    format!("The custom domain node {custom:?}"),
                    "a later milestone (Domain.custom)",
                ));
            }
        }
        Ok(())
    }

    fn condition(
        &mut self,
        condition: &Condition,
        scope: &mut ScopeBuild,
        table: u16,
    ) -> Result<()> {
        let start = scope.tokens.len();
        self.condition_inner(condition, scope, table)?;
        let source = &scope.tables[usize::from(table)];
        let model = self.model(source.model);
        if source.inner
            && let Some(field) = model.field(condition.field_expr())
            && field.has_column()
            && field.company_dependent
            && field.index == Some(erp_fields::IndexKind::BtreeNotNull)
            && self
                .company
                .and_then(|values| values.fallback_matches(model, field, condition))
                == Some(false)
        {
            scope.tokens.splice(
                start..start,
                [
                    Token::Open,
                    Token::MapNotNull(Col {
                        table,
                        field: field.id,
                    }),
                    Token::And,
                ],
            );
            scope.tokens.push(Token::Close);
        }
        Ok(())
    }

    fn condition_inner(
        &mut self,
        condition: &Condition,
        scope: &mut ScopeBuild,
        table: u16,
    ) -> Result<()> {
        let model_id = scope.tables[usize::from(table)].model;
        let model = self.model(model_id);
        let operator = condition.operator();
        let field_expr = condition.field_expr();
        let (name, property) = match field_expr.split_once('.') {
            Some((name, property)) => (name, Some(property)),
            None => (field_expr, None),
        };
        let field = model.field(name).ok_or_else(|| {
            self.input_error(format!(
                "Invalid field {}.{name} in condition {condition:?}",
                model.name
            ))
        })?;
        if operator.is_any()
            && let Some(hierarchy) = crate::relational::HierarchyQuery::of(condition.value())
        {
            let usable = matches!(field.ty, FieldType::Id)
                || (matches!(field.ty, FieldType::Many2one { .. }) && field.has_column());
            if !usable {
                return Err(Error::programming(format!(
                    "a hierarchy sub-query applies to an id or many2one column, not to {}.{} in \
                     {condition:?}",
                    model.name, field.name
                )));
            }
            if field.has_column() {
                self.read(model_id, field);
            }
            // the statement walks the hierarchy: its links are read (flushed first, and a write
            // to them forgets the record checks that depended on them)
            let tree = self.model(hierarchy.model);
            self.read(tree.id, tree.field_by_id(hierarchy.parent));
            // the path column only matters to a walk by path
            let walks_paths = match &hierarchy.start {
                HierarchyStart::Ids(_) => false,
                HierarchyStart::Paths(_) => true,
                HierarchyStart::Selected { by_path, .. } => *by_path,
            };
            if walks_paths && let Some(path) = tree.parent_path {
                self.read(tree.id, tree.field_by_id(path));
            }
            let col = Col {
                table,
                field: field.id,
            };
            let positive = matches!(operator, Operator::Any | Operator::AnyBypass);
            let nullable = can_be_null(field);
            let ids_param = |ids: &[i64]| {
                Param::Array(
                    ScalarKind::Int8,
                    ids.iter().map(|i| Scalar::Int8(*i)).collect(),
                )
            };
            match &hierarchy.start {
                HierarchyStart::Ids(ids) => {
                    scope.tokens.push(Token::Hierarchy {
                        col,
                        positive,
                        model: hierarchy.model,
                        parent: hierarchy.parent,
                        descendants: hierarchy.descendants,
                        by_path: false,
                        nullable,
                    });
                    self.params.push(ids_param(ids));
                }
                HierarchyStart::Paths(paths) => {
                    scope.tokens.push(Token::Hierarchy {
                        col,
                        positive,
                        model: hierarchy.model,
                        parent: hierarchy.parent,
                        descendants: hierarchy.descendants,
                        by_path: true,
                        nullable,
                    });
                    self.params.push(Param::Array(
                        ScalarKind::Text,
                        paths
                            .iter()
                            .map(|p| Scalar::Text(p.as_str().into()))
                            .collect(),
                    ));
                }
                HierarchyStart::Selected {
                    ids,
                    params,
                    by_path,
                } => {
                    if *by_path && (!hierarchy.descendants || tree.parent_path.is_none()) {
                        return Err(Error::programming(format!(
                            "a hierarchy by parent_path applies to child_of on a _parent_store \
                             model, not to {condition:?} on {}",
                            tree.name
                        )));
                    }
                    let id = tree.field("id").ok_or_else(|| {
                        Error::programming(format!("the model {} has no id field", tree.name))
                    })?;
                    // in render order: the literal ids, then one test per parameter
                    let literals = !ids.is_empty();
                    if literals {
                        self.params.push(ids_param(ids));
                    }
                    let mut tests = Vec::with_capacity(params.len());
                    for param in params {
                        tests.push(self.param_member(
                            tree,
                            id,
                            ScalarKind::Int8,
                            param,
                            condition,
                        )?);
                    }
                    scope.tokens.push(Token::HierarchyFrom {
                        col,
                        positive,
                        model: hierarchy.model,
                        parent: hierarchy.parent,
                        descendants: hierarchy.descendants,
                        by_path: *by_path,
                        nullable,
                        literals,
                        params: tests,
                    });
                }
            }
            return Ok(());
        }
        if operator.is_any()
            && let Some(query) = ReferenceQuery::of(condition.value())
        {
            return self.reference_query(condition, query, field, scope, table);
        }
        if operator.is_any() {
            if !field.ty.is_relational() {
                return Err(if matches!(condition.value(), Value::Opaque(_)) {
                    self.not_supported(
                        format!("The query-valued condition {condition:?}"),
                        "a later milestone",
                    )
                } else {
                    Error::programming(format!(
                        "condition_to_sql() operator '{operator}' accepts SQL or Query, got {}",
                        condition.value()
                    ))
                });
            }
            return self.relational(condition, field, scope, table);
        }
        let sql_operator = !matches!(
            operator,
            Operator::EqIfSet | Operator::ChildOf | Operator::ParentOf | Operator::Access
        );
        if sql_operator
            && matches!(condition.value(), Value::Opaque(o) if o.kind() == OpaqueKind::Sql)
        {
            return Err(Error::programming(format!(
                "Unexpected SQL in condition {condition:?}, use Domain.custom() instead"
            )));
        }
        if !operator.is_standard() {
            return Err(Error::programming(format!(
                "Invalid operator '{operator}' for SQL in domain term {condition:?}"
            )));
        }
        if let Some(property) = property {
            return Err(self.not_supported(
                format!(
                    "The property condition {condition:?} ('{property}' of {}.{})",
                    model.name, field.name
                ),
                "a later milestone (date parts and properties,)",
            ));
        }
        if field.ty.is_x2many() {
            return Err(Error::programming(format!(
                "Cannot generate SQL for multi-relational field {}.{} in condition {condition:?}",
                model.name, field.name
            )));
        }
        if !matches!(field.ty, FieldType::Id) && !field.has_column() {
            return Err(self.input_error(format!(
                "Field has no SQL representation because it is not stored in condition \
                 {condition:?}"
            )));
        }
        self.read(model_id, field);
        let col = Col {
            table,
            field: field.id,
        };
        if condition.value().has_param() {
            let token = self.param_condition(model, field, col, condition)?;
            scope.tokens.push(token);
            return Ok(());
        }
        let token = match operator {
            Operator::Overlaps | Operator::Contains => {
                self.range_condition(model, field, col, condition)?
            }
            Operator::In | Operator::NotIn if field.ty.range_bounds().is_some() => {
                self.range_set_condition(model, field, col, condition)?
            }
            Operator::In | Operator::NotIn => self.in_condition(model, field, col, condition)?,
            op if op.is_like() => self.like_condition(model, field, col, condition)?,
            op if op.is_inequality() => self.inequality(model, field, col, condition)?,
            _ => {
                return Err(Error::programming(format!(
                    "Invalid operator '{operator}' for SQL in domain term {condition:?}"
                )));
            }
        };
        scope.tokens.push(token);
        Ok(())
    }

    /// The `any` family on a relational field (the sub-domain is complete, see the module
    /// documentation).
    fn relational(
        &mut self,
        condition: &Condition,
        field: &FieldSpec,
        scope: &mut ScopeBuild,
        table: u16,
    ) -> Result<()> {
        let model_id = scope.tables[usize::from(table)].model;
        let model = self.model(model_id);
        let operator = condition.operator();
        let Value::Domain(sub) = condition.value() else {
            return Err(Error::programming(format!(
                "condition_to_sql() 'any' operator expects a domain once optimized, got {}",
                condition.value()
            )));
        };
        if !field.store {
            return Err(self.input_error(format!(
                "Cannot convert {}.{} to SQL because it is not stored",
                model.name, field.name
            )));
        }
        let positive = matches!(operator, Operator::Any | Operator::AnyBypass);
        let col = Col {
            table,
            field: field.id,
        };
        let outer_alias = Arc::clone(&scope.tables[usize::from(table)].alias);
        match &field.ty {
            FieldType::Many2one { comodel, .. } => {
                let comodel = self.comodel(comodel)?;
                self.read(model_id, field);
                if positive && operator == Operator::AnyBypass {
                    let join = self.join(scope, table, field, comodel);
                    let nullable = can_be_null(field);
                    if nullable {
                        scope.tokens.push(Token::Guard { col, join });
                    }
                    self.node(sub, scope, join)?;
                    if nullable {
                        scope.tokens.push(Token::Close);
                    }
                } else if positive {
                    let mut inner = ScopeBuild::new(comodel, Arc::from(comodel.table.as_str()));
                    self.node(sub, &mut inner, 0)?;
                    scope.tokens.push(Token::Sub(Box::new(SubShape::M2oIn {
                        col,
                        scope: inner.finish(),
                    })));
                } else {
                    let mut inner = ScopeBuild::new(comodel, make_alias(&outer_alias, field.name));
                    self.node(sub, &mut inner, 0)?;
                    scope
                        .tokens
                        .push(Token::Sub(Box::new(SubShape::M2oNotExists {
                            col,
                            scope: inner.finish(),
                        })));
                }
            }
            FieldType::One2many { comodel, inverse } => {
                let comodel = self.comodel(comodel)?;
                let inverse = comodel.field(inverse).ok_or_else(|| {
                    Error::programming(format!(
                        "'{inverse}' declared in {}.{} does not exist on '{}'.",
                        model.name, field.name, comodel.name
                    ))
                })?;
                if !inverse.has_column() {
                    return Err(self.not_supported(
                        format!(
                            "Searching {}.{}, whose inverse is not stored,",
                            model.name, field.name
                        ),
                        "a later milestone",
                    ));
                }
                self.read(comodel.id, inverse);
                let mut inner = ScopeBuild::new(comodel, Arc::from(comodel.table.as_str()));
                self.node(sub, &mut inner, 0)?;
                scope.tokens.push(Token::Sub(Box::new(SubShape::O2m {
                    negated: !positive,
                    outer: table,
                    inverse: inverse.id,
                    scope: inner.finish(),
                })));
            }
            FieldType::Many2many {
                comodel,
                relation: Some(_),
                ..
            } => {
                let comodel = self.comodel(comodel)?;
                self.read(model_id, field);
                let inner = if sub.is_true() {
                    None
                } else {
                    let mut inner = ScopeBuild::new(comodel, Arc::from(comodel.table.as_str()));
                    self.node(sub, &mut inner, 0)?;
                    Some(inner.finish())
                };
                scope.tokens.push(Token::Sub(Box::new(SubShape::M2m {
                    negated: !positive,
                    field: col,
                    alias: make_alias(&outer_alias, field.name),
                    scope: inner,
                })));
            }
            _ => {
                return Err(self.input_error(format!(
                    "Cannot convert {}.{} to SQL because it is not stored",
                    model.name, field.name
                )));
            }
        }
        Ok(())
    }

    /// An `any!` condition on a `many2one_reference` column whose value is a [`ReferenceQuery`]
    /// (Amendment 1): `f IN (SELECT t.id FROM t WHERE <sub>)` on the query's target model,
    /// the sub-query shape of a many2one `any`. Every refusal is a programming error that names
    /// the amendment: a carrier the optimizer did not complete, on another kind of field or
    /// negated means a pass over the predicate did not know it, and compiling it anyway could
    /// let rows through.
    fn reference_query(
        &mut self,
        condition: &Condition,
        query: &ReferenceQuery,
        field: &FieldSpec,
        scope: &mut ScopeBuild,
        table: u16,
    ) -> Result<()> {
        let model_id = scope.tables[usize::from(table)].model;
        let model = self.model(model_id);
        let refuse = |why: String| {
            Error::programming(format!(
                "reference query in condition {condition:?} on {}.{}: {why}",
                model.name, field.name
            ))
        };
        if !matches!(field.ty, FieldType::Many2oneReference { .. }) {
            return Err(refuse(format!(
                "a reference query applies to a many2one_reference column, not to a {} field",
                field.ty.type_name()
            )));
        }
        if condition.operator() != Operator::AnyBypass {
            return Err(refuse(format!(
                "only the positive 'any!' form is compiled, not '{}': a negated reference query \
                 could let rows through",
                condition.operator()
            )));
        }
        if condition.field_expr() != field.name || !field.has_column() {
            return Err(refuse(
                "the condition must name the stored column itself, without a path".to_owned(),
            ));
        }
        if !query.is_optimized() {
            return Err(refuse(
                "its sub-domain was not optimized: a pass over the access predicate did not \
                 descend into the reference query"
                    .to_owned(),
            ));
        }
        let Some(target) = self.ctx.registry.models().get(query.target().index()) else {
            return Err(refuse("its target model is not in the registry".to_owned()));
        };
        if !target.has_table() {
            return Err(refuse(format!(
                "its target model {} has no table (abstract)",
                target.name
            )));
        }
        self.read(model_id, field);
        let col = Col {
            table,
            field: field.id,
        };
        let mut inner = ScopeBuild::new(target, Arc::from(target.table.as_str()));
        self.node(query.sub(), &mut inner, 0)?;
        scope.tokens.push(Token::Sub(Box::new(SubShape::M2oIn {
            col,
            scope: inner.finish(),
        })));
        Ok(())
    }

    fn comodel(&self, name: &str) -> Result<&'a ModelSpec> {
        self.ctx.registry.model(name).ok_or_else(|| {
            Error::programming(format!("Cannot determine the comodel relation '{name}'"))
        })
    }

    /// The join following many2one `field` from `table` (reused when it exists): `JOIN` when the
    /// column is `NOT NULL` and `table` is reached through `JOIN`s only, else `LEFT JOIN`.
    fn join(
        &mut self,
        scope: &mut ScopeBuild,
        table: u16,
        field: &FieldSpec,
        comodel: &ModelSpec,
    ) -> u16 {
        let from = Col {
            table,
            field: field.id,
        };
        if let Some(i) = scope.joins.iter().position(|j| j.from == from) {
            return (i + 1) as u16;
        }
        let parent = &scope.tables[usize::from(table)];
        let inner = parent.inner && !can_be_null(field);
        let link = if field.company_dependent {
            format!("{}__p{}", field.name, self.company_slot.unwrap_or(0))
        } else {
            field.name.to_owned()
        };
        let alias = make_alias(&parent.alias, &link);
        scope.joins.push(JoinShape {
            left: !inner,
            alias: Arc::clone(&alias),
            model: comodel.id,
            from,
        });
        scope.tables.push(TableRef {
            model: comodel.id,
            alias,
            inner,
        });
        scope.joins.len() as u16
    }

    /// `overlaps` / `contains` on a `date_range` or `datetime_range` field (design
    /// §5.2): the operand is a range `[lower, upper]` (`False` for an unbounded end) taken with the
    /// field's bounds, or a point.
    fn range_condition(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let operator = condition.operator();
        let (bounds, element, dates) = match field.ty {
            FieldType::DateRange { bounds } => (bounds, ScalarKind::Date, true),
            FieldType::DatetimeRange { bounds } => (bounds, ScalarKind::Timestamp, false),
            _ => {
                return Err(self.input_error(format!(
                    "operator '{operator}' applies to date_range and datetime_range fields, not \
                     to {}.{} ({}) in condition {condition:?}",
                    model.name,
                    field.name,
                    field.ty.type_name()
                )));
            }
        };
        let value = condition.value();
        if operator == Operator::Contains {
            let point = to_scalar(element, value)
                .map_err(|_| self.invalid_value(model, field, value, condition))?;
            self.params.push(Param::Scalar(point));
            return Ok(Token::RangeContains { col, kind: element });
        }
        let (lower_end, upper_end) = match value {
            Value::List(items) => match items.as_slice() {
                [lower, upper] => (lower, upper),
                _ => return Err(self.invalid_value(model, field, value, condition)),
            },
            other => return Err(self.invalid_value(model, field, other, condition)),
        };
        let end_scalar = |end: &Value| match end {
            Value::Bool(false) => Ok(None),
            other => to_scalar(element, other)
                .map(Some)
                .map_err(|_| self.invalid_value(model, field, value, condition)),
        };
        let (lower, upper) = (end_scalar(lower_end)?, end_scalar(upper_end)?);
        let scalar = if dates {
            let date = |s: Option<Scalar>| match s {
                Some(Scalar::Date(d)) => Ok(Some(d)),
                None => Ok(None),
                Some(_) => Err(self.invalid_value(model, field, value, condition)),
            };
            let range = erp_fields::Range::new(date(lower)?, date(upper)?);
            if inverted(range.lower(), range.upper()) {
                return Err(self.invalid_value(model, field, value, condition));
            }
            Scalar::DateRange(range, bounds)
        } else {
            let stamp = |s: Option<Scalar>| match s {
                Some(Scalar::Timestamp(t)) => Ok(Some(t)),
                None => Ok(None),
                Some(_) => Err(self.invalid_value(model, field, value, condition)),
            };
            let range = erp_fields::Range::new(stamp(lower)?, stamp(upper)?);
            if inverted(range.lower(), range.upper()) {
                return Err(self.invalid_value(model, field, value, condition));
            }
            Scalar::TsRange(range, bounds)
        };
        let kind = scalar.kind();
        self.params.push(Param::Scalar(scalar));
        Ok(Token::RangeOverlaps { col, kind })
    }

    /// `in` / `not in` on a range field: only "set" / "not set" (`False`); ranges are compared
    /// with `overlaps` and `contains`.
    fn range_set_condition(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let positive = condition.operator() == Operator::In;
        let items: Vec<&Value> = match condition.value() {
            Value::Set(set) => set.iter().collect(),
            Value::List(list) => list.iter().collect(),
            other => {
                return Err(Error::programming(format!(
                    "condition_to_sql() 'in' operator expects a collection, not a {other}"
                )));
            }
        };
        if items.is_empty() || !items.iter().all(|v| matches!(v, Value::Bool(false))) {
            return Err(self.input_error(format!(
                "{}.{} is a range field: compare it with 'overlaps' or 'contains' (only = False \
                 and != False test whether it is set), in condition {condition:?}",
                model.name, field.name
            )));
        }
        let null = match (positive, can_be_null(field)) {
            (true, true) => NullTest::IsNull,
            (true, false) => NullTest::False,
            (false, true) => NullTest::IsNotNull,
            (false, false) => NullTest::True,
        };
        Ok(Token::In {
            col,
            positive,
            values: None,
            null,
        })
    }

    fn in_condition(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let positive = condition.operator() == Operator::In;
        let items: Vec<&Value> = match condition.value() {
            Value::Set(set) => set.iter().collect(),
            Value::List(list) => list.iter().collect(),
            other => {
                return Err(Error::programming(format!(
                    "condition_to_sql() 'in' operator expects a collection, not a {other}"
                )));
            }
        };
        if matches!(field.ty, FieldType::Boolean) {
            let has = |b: bool| items.iter().any(|v| v.is_truthy() == b);
            let (with_true, with_false) = if positive {
                (has(true), has(false))
            } else {
                (!has(true), !has(false))
            };
            let test = match (with_true, with_false) {
                (true, true) => BoolTest::True,
                (false, false) => BoolTest::False,
                (true, false) => BoolTest::IsTrue,
                (false, true) => BoolTest::IsNotTrue,
            };
            return Ok(Token::Bool { col, test });
        }
        let Some(kind) = ScalarKind::of_field(&field.ty) else {
            // JSON: only "set" / "not set" tests.
            if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Bool(false))) {
                return Ok(Token::JsonNull {
                    col,
                    is_null: positive,
                });
            }
            return Err(self.not_supported(
                format!(
                    "Searching the JSON field {}.{} with {condition:?}",
                    model.name, field.name
                ),
                "a later milestone",
            ));
        };
        let (values, null) =
            self.literal_in_part(model, field, kind, positive, &items, condition)?;
        Ok(Token::In {
            col,
            positive,
            values,
            null,
        })
    }

    /// The literal members of an `in`/`not in` set: their array parameter (pushed, its kind
    /// returned) and the NULL handling (`False` members and the type's falsy value).
    fn literal_in_part(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        kind: ScalarKind,
        positive: bool,
        items: &[&Value],
        condition: &Condition,
    ) -> Result<(Option<ScalarKind>, NullTest)> {
        let nullable = can_be_null(field);
        let mut params: Vec<Scalar> = Vec::with_capacity(items.len());
        let mut null_in_condition = false;
        for value in items.iter().copied() {
            if matches!(value, Value::Bool(false)) {
                null_in_condition = true;
                continue;
            }
            let scalar = to_scalar(kind, value)
                .map_err(|_| self.invalid_value(model, field, value, condition))?;
            if !params.contains(&scalar) {
                params.push(scalar);
            }
        }
        // A value treated as null (0, 0.0, '') matches NULL rows and the other way round.
        if let Some(null_value) = falsy_value(&field.ty) {
            if params.contains(&null_value) {
                null_in_condition = true;
            } else if null_in_condition {
                params.push(null_value);
            }
        }
        let has_values = !params.is_empty();
        let null = if positive == null_in_condition {
            // field in {v, False}  => field = ANY(v) OR field IS NULL
            // field not in {v}     => field <> ALL(v) OR field IS NULL
            match (nullable, has_values) {
                (false, true) => NullTest::None,
                (false, false) => NullTest::False,
                (true, true) => NullTest::OrIsNull,
                (true, false) => NullTest::IsNull,
            }
        } else if !positive && null_in_condition && !has_values {
            if nullable {
                NullTest::IsNotNull
            } else {
                NullTest::True
            }
        } else if has_values {
            NullTest::None
        } else if positive {
            // Unreachable after optimization (an empty `in` is FALSE); kept total.
            NullTest::False
        } else {
            NullTest::True
        };
        let values = has_values.then(|| {
            self.params.push(Param::Array(kind, params));
            kind
        });
        Ok((values, null))
    }

    /// A condition whose value holds principal parameters: an `in`/`not in` set
    /// of literals and parameters, or an inequality with "now". Parameters compare with ids (and
    /// "now" with dates or datetimes); everything else is refused.
    fn param_condition(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let operator = condition.operator();
        let Some(kind) = ScalarKind::of_field(&field.ty) else {
            return Err(Error::programming(format!(
                "The principal parameters of {condition:?} compare with {}.{}, a {} field: \
                 parameters compare with ids, dates and datetimes only",
                model.name,
                field.name,
                field.ty.type_name()
            )));
        };
        if operator.is_inequality() {
            let Value::Param(PrincipalParam::Now(now)) = condition.value() else {
                return Err(Error::programming(format!(
                    "Invalid principal parameter in the inequality {condition:?}: only \
                     context_today() and datetime.datetime.now() compare with '<', '>', '<=', '>='"
                )));
            };
            let cast = self.check_now(model, field, kind, *now, condition)?;
            let op = match operator {
                Operator::Lt => Inequality::Lt,
                Operator::Gt => Inequality::Gt,
                Operator::Le => Inequality::Le,
                _ => Inequality::Ge,
            };
            return Ok(Token::NowCompare { col, op, now: cast });
        }
        if !matches!(operator, Operator::In | Operator::NotIn) {
            return Err(Error::programming(format!(
                "Invalid operator '{operator}' with a principal parameter in {condition:?}"
            )));
        }
        let positive = operator == Operator::In;
        let items: Vec<&Value> = match condition.value() {
            Value::Set(set) => set.iter().collect(),
            Value::List(list) => list.iter().collect(),
            single @ Value::Param(_) => vec![single],
            other => {
                return Err(Error::programming(format!(
                    "condition_to_sql() 'in' operator expects a collection, not a {other}"
                )));
            }
        };
        let (members, literals): (Vec<&Value>, Vec<&Value>) = items
            .into_iter()
            .partition(|v| matches!(v, Value::Param(_)));
        let (literal_kind, null) = if literals.is_empty() {
            (None, NullTest::None)
        } else {
            self.literal_in_part(model, field, kind, positive, &literals, condition)?
        };
        let mut params = Vec::with_capacity(members.len());
        for member in members {
            if let Value::Param(param) = member {
                params.push(self.param_member(model, field, kind, param, condition)?);
            }
        }
        Ok(Token::ParamIn {
            col,
            positive,
            literals: literal_kind,
            null,
            nullable: can_be_null(field),
            params,
        })
    }

    /// "now" compares with the field type it is typed for (R-S13); its rendering.
    fn check_now(
        &self,
        model: &ModelSpec,
        field: &FieldSpec,
        kind: ScalarKind,
        now: NowKind,
        condition: &Condition,
    ) -> Result<NowCast> {
        let (expected, cast) = match now {
            NowKind::Date => (ScalarKind::Date, NowCast::Date),
            NowKind::Timestamp => (ScalarKind::Timestamp, NowCast::Timestamp),
            other => {
                return Err(Error::programming(format!(
                    "the 'now' parameter {other:?} in {condition:?} has no SQL form"
                )));
            }
        };
        if kind == expected {
            Ok(cast)
        } else {
            Err(Error::programming(format!(
                "The principal parameter {} in {condition:?} compares with {}.{}, a {} field",
                PrincipalParam::Now(now).symbol(),
                model.name,
                field.name,
                field.ty.type_name()
            )))
        }
    }

    /// One parameter member of an `in` set; pushes its statement parameter (in render order).
    fn param_member(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        kind: ScalarKind,
        param: &PrincipalParam,
        condition: &Condition,
    ) -> Result<ParamTest> {
        if let PrincipalParam::Now(now) = param {
            let cast = self.check_now(model, field, kind, *now, condition)?;
            return Ok(ParamTest::Now(cast));
        }
        if kind != ScalarKind::Int8 {
            return Err(Error::programming(format!(
                "The principal parameter {} in {condition:?} compares with {}.{}, a {} field: \
                 it is a record id",
                param.symbol(),
                model.name,
                field.name,
                field.ty.type_name()
            )));
        }
        let slot = |param: PrincipalParam, array: bool| Param::Principal {
            param,
            kind: ScalarKind::Int8,
            array,
        };
        Ok(match param {
            PrincipalParam::Uid | PrincipalParam::CompanyId => {
                self.params.push(slot(param.clone(), false));
                ParamTest::Scalar(ScalarKind::Int8)
            }
            PrincipalParam::CompanyIds | PrincipalParam::GrantIds { .. } => {
                self.params.push(slot(param.clone(), true));
                ParamTest::Array(ScalarKind::Int8)
            }
            PrincipalParam::UserPath(path) => {
                let (shape, set) = self.user_path_shape(path, condition)?;
                // the sub-select's one parameter: the subject
                self.params.push(slot(PrincipalParam::Uid, false));
                if set {
                    ParamTest::PathSet(shape)
                } else {
                    ParamTest::PathScalar(shape)
                }
            }
            other => {
                return Err(Error::programming(format!(
                    "the principal parameter {} in {condition:?} has no SQL form",
                    other.symbol()
                )));
            }
        })
    }

    /// Resolve a user path against the registry: stored many2one hops (a scalar), x2many hops
    /// (a set), ending on an id or a many2one column. Every field read is recorded (flushed
    /// before the statement, R-S5).
    fn user_path_shape(
        &mut self,
        path: &UserPath,
        condition: &Condition,
    ) -> Result<(UserPathShape, bool)> {
        let users = self.ctx.registry.model("res.users").ok_or_else(|| {
            Error::programming(format!(
                "The user path user.{path} in {condition:?} needs the res.users model"
            ))
        })?;
        let refuse = |name: &str, model: &ModelSpec, why: &str| {
            Error::programming(format!(
                "The user path user.{path} in {condition:?} cannot be compiled: {}.{name} {why}",
                model.name
            ))
        };
        let mut model = users;
        let mut hops: Vec<PathHop> = Vec::new();
        let mut set = false;
        let names = path.fields();
        for (i, name) in names.iter().enumerate() {
            let last = i + 1 == names.len();
            if last && &**name == "ids" {
                set = true;
                break;
            }
            let field = model
                .field(name)
                .ok_or_else(|| refuse(name, model, "does not exist"))?;
            match &field.ty {
                FieldType::Id if last => {
                    return Ok((
                        UserPathShape {
                            users: users.id,
                            hops,
                            value: field.id,
                        },
                        set,
                    ));
                }
                FieldType::Many2one { comodel, .. } if field.has_column() => {
                    self.read(model.id, field);
                    if last {
                        return Ok((
                            UserPathShape {
                                users: users.id,
                                hops,
                                value: field.id,
                            },
                            set,
                        ));
                    }
                    let co = self.comodel(comodel)?;
                    hops.push(PathHop {
                        model: model.id,
                        field: field.id,
                        comodel: co.id,
                    });
                    model = co;
                }
                FieldType::One2many { comodel, inverse } if field.store => {
                    let co = self.comodel(comodel)?;
                    let inv = co
                        .field(inverse)
                        .filter(|f| f.has_column())
                        .ok_or_else(|| refuse(name, model, "has no stored inverse"))?;
                    self.read(co.id, inv);
                    hops.push(PathHop {
                        model: model.id,
                        field: field.id,
                        comodel: co.id,
                    });
                    set = true;
                    model = co;
                }
                FieldType::Many2many {
                    comodel,
                    relation: Some(_),
                    ..
                } if field.store => {
                    self.read(model.id, field);
                    let co = self.comodel(comodel)?;
                    hops.push(PathHop {
                        model: model.id,
                        field: field.id,
                        comodel: co.id,
                    });
                    set = true;
                    model = co;
                }
                _ => {
                    return Err(refuse(
                        name,
                        model,
                        "is not a stored relational field (a user path compares record ids)",
                    ));
                }
            }
        }
        // the path ended on an x2many or on `ids`: the ids of the records reached
        let id = model
            .field("id")
            .ok_or_else(|| refuse("id", model, "does not exist"))?;
        Ok((
            UserPathShape {
                users: users.id,
                hops,
                value: id.id,
            },
            set,
        ))
    }

    /// The `like` family: `%value%` unless the operator starts with `=`; non-text columns are cast
    /// to text; `ilike` folds accents when enabled; negations keep NULL rows.
    fn like_condition(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let operator = condition.operator();
        if matches!(
            field.ty,
            FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz
        ) {
            return Err(self.input_error(format!(
                "operator '{operator}' is not supported for native field {}.{} ({})",
                model.name,
                field.name,
                field.ty.type_name()
            )));
        }
        if field.ty.range_bounds().is_some() {
            return Err(self.input_error(format!(
                "operator '{operator}' does not apply to the range field {}.{} in condition \
                 {condition:?}",
                model.name, field.name
            )));
        }
        let value = match condition.value() {
            Value::Str(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            other => return Err(self.invalid_value(model, field, other, condition)),
        };
        let exact = matches!(
            operator,
            Operator::EqLike | Operator::NotEqLike | Operator::EqILike | Operator::NotEqILike
        );
        let pattern = if exact { value } else { format!("%{value}%") };
        self.params
            .push(Param::Scalar(Scalar::Text(pattern.into())));
        let negative = operator.is_negative();
        Ok(Token::Like {
            col,
            insensitive: matches!(operator.positive(), Operator::ILike | Operator::EqILike),
            negative,
            cast: !is_text(&field.ty),
            or_null: negative && can_be_null(field),
        })
    }

    fn inequality(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        col: Col,
        condition: &Condition,
    ) -> Result<Token> {
        let operator = condition.operator();
        let Some(kind) = ScalarKind::of_field(&field.ty) else {
            return Err(Error::programming(format!(
                "Invalid operator '{operator}' for SQL in domain term {condition:?}"
            )));
        };
        let op = match operator {
            Operator::Lt => Inequality::Lt,
            Operator::Gt => Inequality::Gt,
            Operator::Le => Inequality::Le,
            _ => Inequality::Ge,
        };
        let null_value = falsy_value(&field.ty);
        let value = condition.value();
        let scalar = match (&null_value, value) {
            (Some(null), Value::Bool(false)) => null.clone(),
            _ => to_scalar(kind, value)
                .map_err(|_| self.invalid_value(model, field, value, condition))?,
        };
        let or_null = can_be_null(field)
            && null_value
                .as_ref()
                .and_then(|null| null.partial_cmp_same(&scalar))
                .is_some_and(|ordering| match op {
                    Inequality::Lt => ordering.is_lt(),
                    Inequality::Gt => ordering.is_gt(),
                    Inequality::Le => ordering.is_le(),
                    Inequality::Ge => ordering.is_ge(),
                });
        self.params.push(Param::Scalar(scalar));
        Ok(Token::Inequality {
            col,
            op,
            kind,
            or_null,
        })
    }
}

/// Whether a range operand's lower end is after its upper end (`None`: unbounded).
fn inverted<T: Ord>(lower: Option<T>, upper: Option<T>) -> bool {
    matches!((lower, upper), (Some(l), Some(u)) if l > u)
}
