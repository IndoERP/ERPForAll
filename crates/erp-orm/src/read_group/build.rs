//! SQL expressions of group-by specs, aggregates, orders and `having` clauses (GRP §4.2–4.5).
//!
//! A [`Builder`] collects, for one statement, the joins (each alias once, in creation order), the
//! bound parameters (the search predicate's first, then those of sub-queries embedded in joins and
//! of the `having` clause) and the fields read (flushed before the statement runs).

use std::sync::Arc;

use erp_fields::{Error, FieldType, Result, Value};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use erp_sql::filter::{can_be_null, make_alias};
use erp_sql::order::{Nulls, compile_comodel_order};
use erp_sql::param::{Param, Scalar, ScalarKind};
use erp_sql::{literal, quote};
use rustc_hash::FxHashMap;

use super::spec::{
    AggFunc, Dir, Granularity, HAVING_OPERATORS, NullsPos, OrderPart, bad, parse_order,
    parse_spec_or_err,
};
use crate::Transaction;
use crate::env::Env;
use crate::records::Records;
use crate::search::{Compiled, compile_for_at, merge_joins, read_restriction_at};

/// Languages
/// arrive with `res.lang` (M3); until then every environment uses it, as relative dates do.
pub(crate) const WEEK_START: u32 = 7;

/// How a group-by column is post-processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupOut {
    /// The raw value; NULL becomes `False`.
    Scalar,
    /// A `date_part` number: integral floats become integers.
    DatePart,
    /// A record id of this model: a one-record recordset, empty for NULL.
    Records(ModelId),
    /// A translated field JSON map: projected to the context language.
    Translated,
}

/// How an aggregate column is post-processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggOut {
    /// `__count`, `count`, `count_distinct`: NULL becomes 0.
    Count,
    /// NULL becomes `False`.
    Scalar,
    /// `array_agg`: NULL becomes `[]`, NULL items stay `None`.
    Array,
    /// `recordset`: ids deduplicated, NULLs dropped, one prefetch set for the column.
    Records(ModelId),
}

/// A compiled group-by spec.
#[derive(Debug, Clone)]
pub(crate) struct GroupTerm {
    pub(crate) spec: String,
    /// The expression selected.
    pub(crate) expr: String,
    pub(crate) out: GroupOut,
    /// Keys of a selection field of the main model, in definition order (groups sort by them).
    selection: Option<Vec<String>>,
    /// A `day_of_week` part (sorts from the language's first day).
    day_of_week: bool,
    /// A many2one of the main model (an explicit order follows the comodel's order).
    many2one: Option<FieldId>,
}

impl GroupTerm {
    /// The `GROUP BY` item text.
    pub(crate) fn group_item(&self) -> String {
        self.expr.clone()
    }

    /// What ordering by this group sorts on, for the group value `value` (an expression):
    /// selections by declaration order, days of the week from the language's first day.
    pub(crate) fn order_expr(&self, value: &str) -> String {
        if let Some(keys) = &self.selection {
            let keys: Vec<String> = keys.iter().map(|k| literal(k)).collect();
            format!(
                "array_position(ARRAY[{}]::text[], {value}::text)",
                keys.join(", ")
            )
        } else if self.day_of_week {
            format!("mod(7 - {WEEK_START} + {value}::int, 7)")
        } else {
            value.to_owned()
        }
    }
}

/// One term of the order of the groups.
#[derive(Debug, Clone)]
pub(crate) enum OrderItem {
    /// A group-by column (by index).
    Group {
        index: usize,
        dir: Dir,
        nulls: NullsPos,
    },
    /// An aggregate expression.
    Agg {
        expr: String,
        dir: Dir,
        nulls: NullsPos,
    },
    /// A many2one group-by column ordered by its comodel's order.
    Comodel {
        index: usize,
        comodel: ModelId,
        dir: Dir,
        nulls: NullsPos,
    },
}

/// What a grouped statement selects and how it groups and orders.
pub(crate) struct Statement<'s> {
    /// A first column (the `GROUPING()` mask of grouping sets).
    pub(crate) lead: Option<String>,
    pub(crate) groups: &'s [GroupTerm],
    pub(crate) aggs: &'s [AggTerm],
    /// The `GROUP BY` clause (items, or `GROUPING SETS (…)`).
    pub(crate) group_by: Option<String>,
    pub(crate) having: Option<String>,
    pub(crate) order: Vec<OrderItem>,
    /// Add `COUNT(*) OVER ()` (the number of groups before limit and offset).
    pub(crate) count_groups: bool,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: u64,
}

fn directed(expr: &str, dir: Dir, nulls: NullsPos) -> String {
    format!("{expr} {}{}", dir.sql(), nulls.sql())
}

pub(crate) fn count_param(n: u64) -> Result<Param> {
    i64::try_from(n)
        .map(Param::Count)
        .map_err(|_| erp_fields::Error::programming(format!("limit or offset out of range: {n}")))
}

/// A compiled aggregate spec.
#[derive(Debug, Clone)]
pub(crate) struct AggTerm {
    pub(crate) expr: String,
    pub(crate) out: AggOut,
}

pub(crate) struct Builder<'r> {
    pub(crate) registry: &'r Registry,
    pub(crate) env: Env,
    pub(crate) model: &'r ModelSpec,
    pub(crate) external: bool,
    pub(crate) params: Vec<Param>,
    joins: Vec<(String, String)>,
    /// Alias → reached through `JOIN`s only (a many2one join from it may be an inner join).
    inner: FxHashMap<String, bool>,
    /// Fields read by the statement, with their model.
    pub(crate) fields: Vec<(ModelId, FieldId)>,
    /// Fields the group-bys and aggregates name, with their model (the access domain's own reads
    /// are in `fields` only): what field access is checked on.
    pub(crate) touched: Vec<(ModelId, FieldId)>,
    /// The context timezone once checked against PostgreSQL's zones (datetimes are bucketed in
    /// it); `None` until a datetime group-by needs it.
    tz: Option<Option<String>>,
    /// The transaction-validated active companies (review S1), read by
    /// [`Builder::validate_companies`]; the company-bound join binds these, never the
    /// environment's own list.
    companies: Option<Vec<i64>>,
    /// The active companies and their ancestors (sorted), read once by
    /// [`Builder::prepare_closure`] when a parent-matched company-bound join may be built; the
    /// `parent_of` forms bind this instead of the active companies (review FV1).
    closure: Option<Vec<i64>>,
}

fn field_error(external: bool, model: &ModelSpec, name: &str, spec: &str) -> erp_fields::Error {
    bad(
        external,
        format!(
            "Invalid field '{name}' on model '{}' for '{spec}'.",
            model.name
        ),
    )
}

impl<'r> Builder<'r> {
    pub(crate) fn new(
        registry: &'r Registry,
        env: Env,
        model: &'r ModelSpec,
        external: bool,
        filter: &Compiled,
    ) -> Self {
        let tz = None;
        let mut inner = FxHashMap::default();
        inner.insert(model.table.clone(), true);
        let joins: Vec<(String, String)> = filter.join_list.iter().cloned().collect();
        for (alias, text) in &joins {
            inner.insert(alias.clone(), !text.trim_start().starts_with("LEFT"));
        }
        Self {
            registry,
            env,
            model,
            external,
            params: filter.params.clone(),
            joins,
            inner,
            fields: filter.fields.clone(),
            touched: Vec::new(),
            tz,
            companies: None,
            closure: None,
        }
    }

    /// Reads the active companies through the transaction (checked against the newest resolution
    /// of the subject); call before building terms. Skipped in superuser mode, where a related
    /// path is not company-bound.
    ///
    /// # Errors
    ///
    /// As [`Transaction::companies`].
    pub(crate) async fn validate_companies(&mut self, tx: &mut Transaction) -> Result<()> {
        if !self.env.su() {
            let env = self.env.clone();
            self.companies = Some(tx.companies(&env).await?.iter().map(|c| c.get()).collect());
        }
        Ok(())
    }

    /// Reads the ancestor closure of the active companies (`parent_path` of each, cached in the
    /// transaction) when the company-bound related `field` of `model` reaches a hop with a
    /// parent-matched company scope. Raises exactly what the read raises for a damaged path
    /// (`CompanyError::MalformedParentPath`), whatever rows the statement would join (review
    /// FV1). Call before [`Builder::field_sql`]; a no-op in superuser mode, for a field that is
    /// not company-bound and once the closure is known.
    ///
    /// # Errors
    ///
    /// As the read of the company closure.
    pub(crate) async fn prepare_closure(
        &mut self,
        tx: &mut Transaction,
        model: &'r ModelSpec,
        field: &'r FieldSpec,
    ) -> Result<()> {
        if self.closure.is_some() || self.env.su() {
            return Ok(());
        }
        let registry = self.registry;
        let (mut model, mut field) = (model, field);
        while let Some(inherited) = field.inherited {
            if field.has_column() {
                break;
            }
            model = registry.model_by_id(inherited.parent);
            field = model.field_by_id(inherited.parent_field);
        }
        let Some(path) = field
            .related
            .filter(|_| field.company_bound() && !field.store)
        else {
            return Ok(());
        };
        let mut current = model;
        let mut parent_matched = false;
        for name in path.split('.') {
            let Some(f) = current.field(name) else { break };
            if !matches!(f.ty, FieldType::Many2one { .. }) || f.delegate || !f.has_column() {
                break;
            }
            current = crate::relational::comodel_of(registry, f)?;
            parent_matched |= match current.company_scope {
                Some(erp_registry::CompanyScope::Single { matching, .. }) => {
                    erp_registry::CompanyMatch::from(matching).needs_parent_path()
                }
                Some(erp_registry::CompanyScope::Many { matching, .. }) => {
                    erp_registry::CompanyMatch::from(matching).needs_parent_path()
                }
                _ => false,
            };
        }
        if !parent_matched {
            return Ok(());
        }
        let active = self.companies.as_ref().ok_or_else(|| {
            Error::programming(
                "a company-bound group-by was built before the companies were validated",
            )
        })?;
        let env = self.env.clone();
        let mut closure: Vec<i64> = tx
            .company_closure(&env, active)
            .await?
            .into_iter()
            .collect();
        closure.sort_unstable();
        self.closure = Some(closure);
        Ok(())
    }

    /// The joins, in order, each alias once.
    pub(crate) fn join_text(&self) -> String {
        merge_joins(&[&self.joins])
    }

    fn add_join(&mut self, alias: String, text: String, inner: bool) -> Result<()> {
        erp_sql::order::check_join_conflicts(&[&self.joins, &[(alias.clone(), text.clone())]])?;
        if !self.joins.iter().any(|(a, _)| *a == alias) {
            self.inner.insert(alias.clone(), inner);
            self.joins.push((alias, text));
        }
        Ok(())
    }

    fn read(&mut self, model: ModelId, field: FieldId) {
        if !self.touched.contains(&(model, field)) {
            self.touched.push((model, field));
        }
        if !self.fields.contains(&(model, field)) {
            self.fields.push((model, field));
        }
    }

    /// A column the statement reads without the caller reading the field: flushed, not checked.
    fn flush_input(&mut self, model: ModelId, field: FieldId) {
        if !self.fields.contains(&(model, field)) {
            self.fields.push((model, field));
        }
    }

    fn is_inner(&self, alias: &str) -> bool {
        self.inner.get(alias).copied().unwrap_or(false)
    }

    fn plain_join(&mut self, model: &ModelSpec, alias: &str, field: &FieldSpec) -> Result<String> {
        let comodel = crate::relational::comodel_of(self.registry, field)?;
        let coalias = make_alias(alias, field.name).to_string();
        let inner = self.is_inner(alias) && !can_be_null(field);
        let kind = if inner { "JOIN" } else { "LEFT JOIN" };
        self.read(model.id, field.id);
        let text = format!(
            " {kind} {} AS {} ON ({}.{} = {}.{})",
            quote(&comodel.table),
            quote(&coalias),
            quote(alias),
            quote(field.name),
            quote(&coalias),
            quote("id")
        );
        self.add_join(coalias.clone(), text, inner)?;
        Ok(coalias)
    }

    /// The join of a hop of a company-bound related path: the comodel's rows
    /// restricted to the reader's active companies (or to none), so a record of another company
    /// falls into the empty group like a hidden target of a many2one group-by. A comodel without
    /// company scope is joined plainly.
    fn company_bound_join(
        &mut self,
        model: &ModelSpec,
        alias: &str,
        field: &FieldSpec,
    ) -> Result<String> {
        let registry = self.registry;
        let comodel = crate::relational::comodel_of(registry, field)?;
        let coalias = make_alias(alias, &format!("{}__company", field.name)).to_string();
        if self.joins.iter().any(|(a, _)| *a == coalias) {
            return Ok(coalias);
        }
        let co_table = quote(&comodel.table);
        // the placeholder is only known once the parameter is pushed; build the condition on a
        // probe first so a model without company scope pushes nothing
        let placeholder = format!("${}::int8[]", self.params.len() + 1);
        let Some(pred) =
            crate::related_company::within_companies_sql(comodel, &comodel.table, &placeholder)?
        else {
            return self.plain_join(model, alias, field);
        };
        // the companies the hop may belong to, bound here (this statement is not compiled by the
        // search path that binds principal parameters): the validated active companies of the
        // environment, and for a parent form also their ancestors, read by `prepare_closure`
        let parent_matched = match comodel.company_scope {
            Some(erp_registry::CompanyScope::Single { matching, .. }) => {
                erp_registry::CompanyMatch::from(matching).needs_parent_path()
            }
            Some(erp_registry::CompanyScope::Many { matching, .. }) => {
                erp_registry::CompanyMatch::from(matching).needs_parent_path()
            }
            _ => false,
        };
        let usable = if parent_matched {
            self.closure.as_ref().ok_or_else(|| {
                Error::programming(
                    "a parent-matched company-bound group-by join was built before the company \
                     closure was read",
                )
            })?
        } else {
            self.companies.as_ref().ok_or_else(|| {
                Error::programming(
                    "a company-bound group-by join was built before the companies were validated",
                )
            })?
        };
        self.params.push(Param::Array(
            ScalarKind::Int8,
            usable.iter().map(|c| Scalar::Int8(*c)).collect(),
        ));
        self.read(model.id, field.id);
        // the statement reads the hop's company column (or its link table) and, for the parent
        // forms, the companies' parent paths: pending writes to them are flushed first (review
        // RC4); they are inputs of the join, not fields the caller reads
        if let Some(
            erp_registry::CompanyScope::Single { field: company, .. }
            | erp_registry::CompanyScope::Many { field: company, .. },
        ) = comodel.company_scope
        {
            self.flush_input(comodel.id, company);
        }
        let matching = match comodel.company_scope {
            Some(erp_registry::CompanyScope::Single { matching, .. }) => {
                Some(erp_registry::CompanyMatch::from(matching))
            }
            Some(erp_registry::CompanyScope::Many { matching, .. }) => {
                Some(erp_registry::CompanyMatch::from(matching))
            }
            None | Some(erp_registry::CompanyScope::Itself) => None,
            Some(other) => {
                return Err(crate::related_company::unknown_scope(other, comodel.name));
            }
        };
        if matching.is_some_and(erp_registry::CompanyMatch::needs_parent_path)
            && let Some(companies) = registry.model("res.company")
            && let Some(path) = companies.parent_path
        {
            self.flush_input(companies.id, path);
        }
        let text = format!(
            " LEFT JOIN (SELECT {co_table}.* FROM {co_table} WHERE {pred}) AS {} ON ({}.{} = {}.{})",
            quote(&coalias),
            quote(alias),
            quote(field.name),
            quote(&coalias),
            quote("id")
        );
        self.add_join(coalias.clone(), text, false)?;
        Ok(coalias)
    }

    pub(crate) fn field_sql(
        &mut self,
        model: &'r ModelSpec,
        alias: &str,
        field: &'r FieldSpec,
    ) -> Result<String> {
        if field.ty.is_x2many() {
            return Err(bad(
                self.external,
                format!(
                    "Cannot generate SQL for multi-relational field {}.{}",
                    model.name, field.name
                ),
            ));
        }
        if matches!(field.ty, FieldType::Id) {
            return Ok(format!("{}.{}", quote(alias), quote("id")));
        }
        // Keep the caller's exposure for access checks when expansion reads another field.
        // Inherited generated metadata names the owner; its child field must not be flushed
        // as an ordinary child column, so this is touched-only tracking.
        if (field.related.is_some() || (field.is_generated() && field.inherited.is_some()))
            && !self.touched.contains(&(model.id, field.id))
        {
            self.touched.push((model.id, field.id));
        }
        if field.has_column() {
            self.read(model.id, field.id);
            return Ok(format!("{}.{}", quote(alias), quote(field.name)));
        }
        if let Some(inherited) = field.inherited {
            let registry = self.registry;
            let delegate = model.field_by_id(inherited.delegate);
            let parent_alias = self.plain_join(model, alias, delegate)?;
            let parent = registry.model_by_id(inherited.parent);
            let parent_field = parent.field_by_id(inherited.parent_field);
            return self.field_sql(parent, &parent_alias, parent_field);
        }
        if let Some(path) = field.related.filter(|_| !field.store) {
            let registry = self.registry;
            let mut current: &ModelSpec = model;
            let mut current_alias = alias.to_owned();
            let mut segments = path.split('.').peekable();
            while let Some(name) = segments.next() {
                let f = current.field(name).ok_or_else(|| {
                    bad(
                        self.external,
                        format!(
                            "Invalid related path {path} on {}.{}",
                            model.name, field.name
                        ),
                    )
                })?;
                if segments.peek().is_none() {
                    // the record a bound many2one ends on is one more hop (review SC4)
                    if field.company_bound()
                        && !self.env.su()
                        && !f.delegate
                        && matches!(f.ty, FieldType::Many2one { .. })
                        && f.has_column()
                        && crate::relational::comodel_of(registry, f)?
                            .company_scope
                            .is_some_and(|scope| {
                                !matches!(scope, erp_registry::CompanyScope::Itself)
                            })
                    {
                        let joined = self.company_bound_join(current, &current_alias, f)?;
                        return Ok(format!("{}.{}", quote(&joined), quote("id")));
                    }
                    return self.field_sql(current, &current_alias, f);
                }
                if !matches!(f.ty, FieldType::Many2one { .. }) {
                    break;
                }
                current_alias = if field.company_bound() && !self.env.su() && !f.delegate {
                    self.company_bound_join(current, &current_alias, f)?
                } else {
                    self.plain_join(current, &current_alias, f)?
                };
                current = crate::relational::comodel_of(registry, f)?;
            }
        }
        Err(bad(
            self.external,
            format!(
                "Cannot convert {}.{} to SQL because it is not stored",
                model.name, field.name
            ),
        ))
    }

    async fn group_join(
        &mut self,
        tx: &mut Transaction,
        model: &ModelSpec,
        alias: &str,
        field: &FieldSpec,
    ) -> Result<String> {
        let registry = self.registry;
        let comodel = crate::relational::comodel_of(registry, field)?;
        let bypass = self.env.su() || field.delegate || field.compute_sudo();
        if !bypass {
            let co = Records::empty(self.env.clone(), comodel.id);
            if let Some(restriction) = read_restriction_at(tx, &co, self.params.len()).await? {
                let coalias =
                    make_alias(alias, &format!("{}__{}", field.name, self.env.uid())).to_string();
                if !self.joins.iter().any(|(a, _)| *a == coalias) {
                    self.read(model.id, field.id);
                    self.fields.extend(restriction.fields.iter().copied());
                    self.params.extend(restriction.params.iter().cloned());
                    let co_table = quote(&comodel.table);
                    let pred = match restriction.constant {
                        Some(false) => "FALSE".to_owned(),
                        _ => restriction.sql.to_string(),
                    };
                    let text = format!(
                        " LEFT JOIN (SELECT {co_table}.* FROM {co_table}{} WHERE {pred}) AS {} ON \
                         ({}.{} = {}.{})",
                        merge_joins(&[&restriction.join_list]),
                        quote(&coalias),
                        quote(alias),
                        quote(field.name),
                        quote(&coalias),
                        quote("id")
                    );
                    self.add_join(coalias.clone(), text, false)?;
                }
                return Ok(coalias);
            }
        }
        self.plain_join(model, alias, field)
    }

    async fn many2many_join(
        &mut self,
        tx: &mut Transaction,
        model: &ModelSpec,
        alias: &str,
        field: &FieldSpec,
    ) -> Result<(String, ModelId)> {
        let registry = self.registry;
        let FieldType::Many2many {
            relation: Some(rel),
            ..
        } = &field.ty
        else {
            return Err(bad(
                self.external,
                format!(
                    "Cannot join a non-stored many2many field: {}.{}",
                    model.name, field.name
                ),
            ));
        };
        let comodel = crate::relational::comodel_of(registry, field)?;
        let rel_alias = make_alias(alias, &format!("{}__rel", field.name)).to_string();
        let expr = format!("{}.{}", quote(&rel_alias), quote(&rel.column2));
        if self.joins.iter().any(|(a, _)| *a == rel_alias) {
            return Ok((expr, comodel.id));
        }
        let env = tx.field_env(&self.env, model.id, field.id);
        let env = if field.bypass_search_access {
            env.sudo(true)
        } else {
            env
        };
        let codomain = tx.shared.relations.domain(model.id, field.id);
        let co = Records::empty(env, comodel.id);
        let coquery = compile_for_at(tx, &co, &codomain, true, self.params.len()).await?;
        let mut condition = format!(
            "{}.{} = {}.{}",
            quote(alias),
            quote("id"),
            quote(&rel_alias),
            quote(&rel.column1)
        );
        match coquery.constant {
            Some(true) => {}
            Some(false) => condition.push_str(" AND FALSE"),
            None => {
                self.params.extend(coquery.params.iter().cloned());
                self.fields.extend(coquery.fields.iter().copied());
                let co_table = quote(&comodel.table);
                condition.push_str(&format!(
                    " AND {expr} IN (SELECT {co_table}.{} FROM {co_table}{} WHERE {})",
                    quote("id"),
                    merge_joins(&[&coquery.join_list]),
                    coquery.sql
                ));
            }
        }
        let text = format!(
            " LEFT JOIN {} AS {} ON ({condition})",
            quote(&rel.table),
            quote(&rel_alias)
        );
        self.add_join(rel_alias, text, false)?;
        Ok((expr, comodel.id))
    }

    /// A date or datetime field bucketed or split by `gran` (`fields_temporal.property_to_sql`):
    /// datetimes are first moved to the context timezone.
    async fn group_tz(&mut self, tx: &mut Transaction) -> Result<Option<String>> {
        if let Some(tz) = &self.tz {
            return Ok(tz.clone());
        }
        let resolved = match self.env.context().tz() {
            None => None,
            Some(tz) if tx.pg_timezone_known(tz).await? => Some(tz.to_owned()),
            Some(tz) => {
                tracing::warn!(target: "erp::orm", tz, "grouping in an unknown timezone; using UTC");
                None
            }
        };
        self.tz = Some(resolved.clone());
        Ok(resolved)
    }

    fn date_sql(&self, column: &str, datetime: bool, gran: Granularity) -> String {
        let mut expr = column.to_owned();
        if datetime && let Some(Some(tz)) = &self.tz {
            expr = format!("timezone({}, timezone('UTC', {expr}))", literal(tz));
        }
        if let Some(part) = gran.date_part() {
            return format!("date_part('{part}', {expr})");
        }
        let expr = if gran == Granularity::Week {
            // first_week_day: 0 = Monday … 6 = Sunday
            let first = WEEK_START - 1;
            let offset = if first == 0 { 0 } else { 7 - first };
            format!(
                "(date_trunc('week', {expr}::timestamp - INTERVAL '-{offset} DAY') + INTERVAL \
                 '-{offset} DAY')"
            )
        } else {
            format!("date_trunc('{}', {expr}::timestamp)", gran.name())
        };
        if datetime {
            expr
        } else {
            format!("{expr}::date")
        }
    }

    pub(crate) fn groupby_term<'a>(
        &'a mut self,
        tx: &'a mut Transaction,
        model: &'r ModelSpec,
        alias: String,
        spec: String,
        full_spec: &'a str,
    ) -> crate::dispatch::BoxFuture<'a, Result<GroupTerm>> {
        Box::pin(async move {
            let external = self.external;
            let parts = parse_spec_or_err(&spec, external)?;
            let field = model.field(parts.field).ok_or_else(|| {
                bad(
                    external,
                    format!("Invalid field '{}' on model '{}'", parts.field, model.name),
                )
            })?;
            let main = std::ptr::eq(model, self.model);
            let mut term = GroupTerm {
                spec: full_spec.to_owned(),
                expr: String::new(),
                out: if field.translate {
                    GroupOut::Translated
                } else {
                    GroupOut::Scalar
                },
                selection: None,
                day_of_week: false,
                many2one: None,
            };
            if let Some(path) = parts.path {
                if !matches!(field.ty, FieldType::Many2one { .. }) {
                    return Err(bad(
                        external,
                        format!(
                            "Only many2one path is accepted for the '{full_spec}' groupby spec"
                        ),
                    ));
                }
                let registry = self.registry;
                let comodel = crate::relational::comodel_of(registry, field)?;
                let coalias = self.group_join(tx, model, &alias, field).await?;
                let sub = match parts.func {
                    Some(func) => format!("{path}:{func}"),
                    None => path.to_owned(),
                };
                let inner = self
                    .groupby_term(tx, comodel, coalias, sub, full_spec)
                    .await?;
                term.expr = inner.expr;
                term.out = inner.out;
                return Ok(term);
            }
            self.prepare_closure(tx, model, field).await?;
            let mut granularity = parts.func;
            match &field.ty {
                FieldType::Many2many { .. } => {
                    let (expr, comodel) = self.many2many_join(tx, model, &alias, field).await?;
                    term.expr = expr;
                    term.out = GroupOut::Records(comodel);
                }
                FieldType::Date | FieldType::Datetime => {
                    let Some(name) = granularity.take() else {
                        return Err(bad(
                            external,
                            format!("Granularity not set on a date(time) field: '{full_spec}'"),
                        ));
                    };
                    let gran = Granularity::parse(name).ok_or_else(|| {
                        bad(
                            external,
                            format!(
                                "Error when processing the granularity {name} is not supported. \
                                 Only hour, day, week, month, quarter, year, year_number, \
                                 quarter_number, month_number, iso_week_number, day_of_year, \
                                 day_of_month, day_of_week, hour_number, minute_number, \
                                 second_number are supported"
                            ),
                        )
                    })?;
                    let column = self.field_sql(model, &alias, field)?;
                    let datetime = matches!(field.ty, FieldType::Datetime);
                    if datetime {
                        self.group_tz(tx).await?;
                    }
                    term.expr = self.date_sql(&column, datetime, gran);
                    if !gran.is_bucket() {
                        term.out = GroupOut::DatePart;
                        term.day_of_week = gran == Granularity::DayOfWeek && main;
                    }
                }
                FieldType::Boolean => {
                    let column = self.field_sql(model, &alias, field)?;
                    term.expr = format!("COALESCE({column}, FALSE)");
                }
                // a range has no group value: group by its ends' fields instead
                FieldType::DateRange { .. } | FieldType::DatetimeRange { .. } => {
                    return Err(bad(
                        external,
                        format!("Cannot group by the range field '{full_spec}'"),
                    ));
                }
                FieldType::Many2one { .. } => {
                    term.expr = self.field_sql(model, &alias, field)?;
                    let comodel = crate::relational::comodel_of(self.registry, field)?;
                    term.out = GroupOut::Records(comodel.id);
                    if main {
                        term.many2one = Some(field.id);
                    }
                }
                FieldType::Id => {
                    term.expr = self.field_sql(model, &alias, field)?;
                    term.out = GroupOut::Records(model.id);
                }
                FieldType::Selection { values, .. } => {
                    term.expr = self.field_sql(model, &alias, field)?;
                    if main {
                        term.selection = values
                            .as_ref()
                            .map(|v| v.iter().map(|(k, _)| k.clone()).collect());
                    }
                }
                _ => term.expr = self.field_sql(model, &alias, field)?,
            }
            if granularity.is_some() {
                return Err(bad(
                    external,
                    format!("Granularity set on a no-datetime field or property: '{full_spec}'"),
                ));
            }
            Ok(term)
        })
    }

    /// Reads the company closure an aggregate over a company-bound related field needs (see
    /// [`Builder::prepare_closure`]); a spec that does not name a field of the model is left for
    /// [`Builder::aggregate_term`] to refuse.
    ///
    /// # Errors
    ///
    /// As [`Builder::prepare_closure`].
    pub(crate) async fn prepare_aggregate(
        &mut self,
        tx: &mut Transaction,
        spec: &str,
    ) -> Result<()> {
        let model = self.model;
        if let Ok(parts) = parse_spec_or_err(spec, self.external)
            && parts.path.is_none()
            && let Some(field) = model.field(parts.field)
        {
            self.prepare_closure(tx, model, field).await?;
        }
        Ok(())
    }

    pub(crate) fn aggregate_term(&mut self, spec: &str) -> Result<AggTerm> {
        let external = self.external;
        if spec == "__count" {
            return Ok(AggTerm {
                expr: "COUNT(*)".to_owned(),
                out: AggOut::Count,
            });
        }
        let parts = parse_spec_or_err(spec, external)?;
        if parts.path.is_some() {
            return Err(bad(
                external,
                format!("Invalid '{spec}', this dot notation is not supported"),
            ));
        }
        let model = self.model;
        let field = model
            .field(parts.field)
            .ok_or_else(|| field_error(external, model, parts.field, spec))?;
        let Some(func_name) = parts.func else {
            return Err(bad(
                external,
                format!("Aggregate method is mandatory for '{}'", parts.field),
            ));
        };
        let func = AggFunc::parse(func_name).ok_or_else(|| {
            bad(
                external,
                format!("Invalid aggregate method '{func_name}' for '{spec}'."),
            )
        })?;
        if func == AggFunc::SumCurrency {
            if !matches!(field.ty, FieldType::Monetary { .. }) {
                return Err(bad(
                    external,
                    format!(
                        "Aggregator \"sum_currency\" only works on currency field for '{}'",
                        parts.field
                    ),
                ));
            }
            return Err(erp_fields::Error::programming(
                "the 'sum_currency' aggregate needs currency rates (res.currency): not supported \
                 until M3",
            ));
        }
        if func == AggFunc::Recordset && !(field.ty.is_relational() || field.name == "id") {
            return Err(bad(
                external,
                format!(
                    "Aggregate method 'recordset' can be only used on relational field (or id) \
                     (for '{spec}')."
                ),
            ));
        }
        let table = model.table.clone();
        let column = self.field_sql(model, &table, field)?;
        let id = format!("{}.{}", quote(&table), quote("id"));
        let (expr, out) = match func {
            AggFunc::Sum => (format!("SUM({column})"), AggOut::Scalar),
            AggFunc::Avg => (format!("AVG({column})"), AggOut::Scalar),
            AggFunc::Max => (format!("MAX({column})"), AggOut::Scalar),
            AggFunc::Min => (format!("MIN({column})"), AggOut::Scalar),
            AggFunc::BoolAnd => (format!("BOOL_AND({column})"), AggOut::Scalar),
            AggFunc::BoolOr => (format!("BOOL_OR({column})"), AggOut::Scalar),
            AggFunc::ArrayAgg => (format!("ARRAY_AGG({column} ORDER BY {id})"), AggOut::Array),
            AggFunc::ArrayAggDistinct => (
                format!("ARRAY_AGG(DISTINCT {column} ORDER BY {column})"),
                AggOut::Array,
            ),
            AggFunc::Recordset => {
                let target = match field.ty.comodel() {
                    Some(_) => crate::relational::comodel_of(self.registry, field)?.id,
                    None => model.id,
                };
                (
                    format!(
                        "ARRAY_AGG({column} ORDER BY {id}) FILTER (WHERE {column} IS NOT NULL)"
                    ),
                    AggOut::Records(target),
                )
            }
            AggFunc::Count => (format!("COUNT({column})"), AggOut::Count),
            AggFunc::CountDistinct => (format!("COUNT(DISTINCT {column})"), AggOut::Count),
            AggFunc::SumCurrency => unreachable_sum_currency()?,
        };
        Ok(AggTerm { expr, out })
    }

    pub(crate) fn orderby(
        &mut self,
        order: Option<&str>,
        terms: &[GroupTerm],
    ) -> Result<Vec<OrderItem>> {
        let explicit = order.is_some_and(|o| !o.trim().is_empty());
        let parts: Vec<OrderPart> = if explicit {
            parse_order(order.unwrap_or_default(), self.external)?
        } else {
            let mut seen: Vec<&str> = Vec::new();
            terms
                .iter()
                .filter(|t| {
                    let new = !seen.contains(&t.spec.as_str());
                    seen.push(&t.spec);
                    new
                })
                .map(|t| OrderPart {
                    term: t.spec.clone(),
                    dir: Dir::Asc,
                    nulls: NullsPos::Default,
                })
                .collect()
        };
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            let Some(index) = terms.iter().position(|t| t.spec == part.term) else {
                // the aggregate's own error, unless the term is not even a spec
                if super::spec::parse_spec(&part.term).is_none() {
                    return Err(bad(
                        self.external,
                        format!(
                            "Order term '{}' is not a valid aggregate nor valid groupby",
                            part.term
                        ),
                    ));
                }
                let agg = self.aggregate_term(&part.term)?;
                out.push(OrderItem::Agg {
                    expr: agg.expr,
                    dir: part.dir,
                    nulls: part.nulls,
                });
                continue;
            };
            let term = &terms[index];
            if let Some(fid) = term.many2one.filter(|_| explicit) {
                let registry = self.registry;
                let field = self.model.field_by_id(fid);
                let comodel = crate::relational::comodel_of(registry, field)?;
                if comodel.order.trim() != "id" {
                    out.push(OrderItem::Comodel {
                        index,
                        comodel: comodel.id,
                        dir: part.dir,
                        nulls: part.nulls,
                    });
                    continue;
                }
            }
            out.push(OrderItem::Group {
                index,
                dir: part.dir,
                nulls: part.nulls,
            });
        }
        Ok(out)
    }

    /// The SQL of a grouped read. With a many2one ordered by its comodel's order the rows are
    /// grouped in a sub-query (by the ids alone) and the comodel is joined around it for the
    /// order, the limit, the offset and the window count; otherwise one flat statement.
    pub(crate) fn render(&mut self, filter: &Compiled, st: &Statement<'_>) -> Result<String> {
        let table = quote(&self.model.table);
        let outer = st
            .order
            .iter()
            .any(|o| matches!(o, OrderItem::Comodel { .. }));
        let mut tail = String::new();
        let mut where_ = String::new();
        if filter.constant != Some(true) {
            where_.push_str(" WHERE ");
            where_.push_str(&filter.sql);
        }
        let mut grouping = String::new();
        if let Some(group_by) = &st.group_by {
            grouping.push_str(" GROUP BY ");
            grouping.push_str(group_by);
            if let Some(having) = &st.having {
                grouping.push_str(" HAVING ");
                grouping.push_str(having);
            }
        }
        let sql = if !outer {
            let mut select: Vec<String> = st.lead.iter().cloned().collect();
            select.extend(st.groups.iter().map(|t| t.expr.clone()));
            select.extend(st.aggs.iter().map(|a| a.expr.clone()));
            if st.count_groups {
                select.push("COUNT(*) OVER ()".to_owned());
            }
            let order: Vec<String> = st
                .order
                .iter()
                .map(|o| match o {
                    OrderItem::Group { index, dir, nulls } => st
                        .groups
                        .get(*index)
                        .map(|t| directed(&t.order_expr(&t.expr), *dir, *nulls))
                        .unwrap_or_default(),
                    OrderItem::Agg { expr, dir, nulls } => directed(expr, *dir, *nulls),
                    OrderItem::Comodel { .. } => String::new(),
                })
                .collect();
            if !order.is_empty() {
                tail.push_str(" ORDER BY ");
                tail.push_str(&order.join(", "));
            }
            format!(
                "SELECT {} FROM {table}{}{where_}{grouping}{tail}",
                select.join(", "),
                self.join_text()
            )
        } else {
            let col = |i: usize| format!("\"__g\".{}", quote(&format!("c{i}")));
            let mut inner: Vec<String> = Vec::new();
            let mut outer_select: Vec<String> = Vec::new();
            if let Some(lead) = &st.lead {
                inner.push(format!("{lead} AS \"__m\""));
                outer_select.push("\"__g\".\"__m\"".to_owned());
            }
            for (i, t) in st.groups.iter().enumerate() {
                inner.push(format!("{} AS {}", t.expr, quote(&format!("c{i}"))));
                outer_select.push(col(i));
            }
            let n = st.groups.len();
            for (j, a) in st.aggs.iter().enumerate() {
                inner.push(format!("{} AS {}", a.expr, quote(&format!("c{}", n + j))));
                outer_select.push(col(n + j));
            }
            if st.count_groups {
                outer_select.push("COUNT(*) OVER ()".to_owned());
            }
            let mut joins: Vec<(String, String)> = Vec::new();
            let mut order: Vec<String> = Vec::new();
            for (k, item) in st.order.iter().enumerate() {
                match item {
                    OrderItem::Group { index, dir, nulls } => {
                        if let Some(t) = st.groups.get(*index) {
                            order.push(directed(&t.order_expr(&col(*index)), *dir, *nulls));
                        }
                    }
                    OrderItem::Agg { expr, dir, nulls } => {
                        let name = format!("o{k}");
                        inner.push(format!("{expr} AS {}", quote(&name)));
                        order.push(directed(&format!("\"__g\".{}", quote(&name)), *dir, *nulls));
                    }
                    OrderItem::Comodel {
                        index,
                        comodel,
                        dir,
                        nulls,
                    } => {
                        let registry = self.registry;
                        let comodel = registry.model_by_id(*comodel);
                        let compiled = compile_comodel_order(
                            registry,
                            comodel,
                            &col(*index),
                            &format!("__g__o{index}"),
                            *dir == Dir::Desc,
                            match nulls {
                                NullsPos::Default => Nulls::Default,
                                NullsPos::First => Nulls::First,
                                NullsPos::Last => Nulls::Last,
                            },
                            self.env.lang(),
                        )?;
                        for (alias, text) in compiled.joins {
                            if !joins.iter().any(|(a, _)| *a == alias) {
                                joins.push((alias, text));
                            }
                        }
                        for (m, f) in compiled.fields {
                            self.read(m, f);
                        }
                        order.extend(compiled.parts.into_iter().map(|p| p.sql));
                    }
                }
            }
            if !order.is_empty() {
                tail.push_str(" ORDER BY ");
                tail.push_str(&order.join(", "));
            }
            format!(
                "SELECT {} FROM (SELECT {} FROM {table}{}{where_}{grouping}) AS \"__g\"{}{tail}",
                outer_select.join(", "),
                inner.join(", "),
                self.join_text(),
                merge_joins(&[&joins])
            )
        };
        let mut sql = sql;
        if let Some(limit) = st.limit {
            self.params.push(count_param(limit)?);
            sql.push_str(&format!(" LIMIT ${}::int8", self.params.len()));
        }
        if st.offset > 0 {
            self.params.push(count_param(st.offset)?);
            sql.push_str(&format!(" OFFSET ${}::int8", self.params.len()));
        }
        Ok(sql)
    }

    pub(crate) fn having(&mut self, having: &[Value]) -> Result<Option<String>> {
        if having.is_empty() {
            return Ok(None);
        }
        let external = self.external;
        // conditions are rendered first, in order, so placeholders follow the clause
        enum Item {
            Not,
            And,
            Or,
            Cond(String),
        }
        let mut items = Vec::with_capacity(having.len());
        for item in having {
            items.push(match item {
                Value::Str(op) if &**op == "!" => Item::Not,
                Value::Str(op) if &**op == "&" => Item::And,
                Value::Str(op) if &**op == "|" => Item::Or,
                Value::List(triple) if triple.len() == 3 => {
                    let (Value::Str(left), Value::Str(op)) = (&triple[0], &triple[1]) else {
                        return Err(invalid_having(external, item));
                    };
                    if !HAVING_OPERATORS.contains(&&**op) {
                        return Err(bad(
                            external,
                            format!(
                                "Invalid having clause {item}: supported comparators are \
                                 ('in', 'not in', '<', '>', '<=', '>=', '=', '!=')"
                            ),
                        ));
                    }
                    let agg = self.aggregate_term(left)?;
                    let right = self.having_value(&triple[2], op, item)?;
                    Item::Cond(match &**op {
                        "in" => format!("{} = ANY({right})", agg.expr),
                        "not in" => format!("{} <> ALL({right})", agg.expr),
                        "!=" => format!("{} <> {right}", agg.expr),
                        other => format!("{} {other} {right}", agg.expr),
                    })
                }
                other => return Err(invalid_having(external, other)),
            });
        }
        let mut stack: Vec<String> = Vec::new();
        for item in items.into_iter().rev() {
            match item {
                Item::Not => {
                    let a = pop(&mut stack, external)?;
                    stack.push(format!("(NOT {a})"));
                }
                Item::And | Item::Or => {
                    let a = pop(&mut stack, external)?;
                    let b = pop(&mut stack, external)?;
                    let word = if matches!(item, Item::And) {
                        "AND"
                    } else {
                        "OR"
                    };
                    stack.push(format!("({a} {word} {b})"));
                }
                Item::Cond(sql) => stack.push(sql),
            }
        }
        while stack.len() > 1 {
            let a = pop(&mut stack, external)?;
            let b = pop(&mut stack, external)?;
            stack.push(format!("({a} AND {b})"));
        }
        Ok(stack.pop())
    }

    fn having_value(&mut self, value: &Value, op: &str, item: &Value) -> Result<String> {
        let scalar = |v: &Value| -> Option<Scalar> {
            Some(match v {
                Value::Int(i) => Scalar::Int8(*i),
                Value::Float(f) if f.is_finite() => Scalar::Float8(*f),
                Value::Decimal(d) => Scalar::Numeric(*d),
                Value::Str(s) => Scalar::Text(Arc::clone(s)),
                Value::Date(d) => Scalar::Date(*d),
                Value::DateTime(d) => Scalar::Timestamp(*d),
                Value::Uuid(v) => Scalar::Uuid(*v),
                Value::Xid8(v) => Scalar::Xid8(*v),
                Value::TimestampTz(v) => Scalar::TimestampTz(*v),
                _ => return None,
            })
        };
        let param = if op == "in" || op == "not in" {
            let Value::List(items) = value else {
                return Err(invalid_having(self.external, item));
            };
            let items: Vec<Scalar> = items
                .iter()
                .map(|v| scalar(v).ok_or_else(|| invalid_having(self.external, item)))
                .collect::<Result<_>>()?;
            let kind = items.first().map_or(ScalarKind::Int8, Scalar::kind);
            if items.iter().any(|s| s.kind() != kind) {
                return Err(invalid_having(self.external, item));
            }
            Param::Array(kind, items)
        } else {
            match value {
                Value::Bool(b) => return Ok(if *b { "TRUE" } else { "FALSE" }.to_owned()),
                other => {
                    Param::Scalar(scalar(other).ok_or_else(|| invalid_having(self.external, item))?)
                }
            }
        };
        let cast = param.sql_type();
        self.params.push(param);
        Ok(format!("${}::{cast}", self.params.len()))
    }
}

fn unreachable_sum_currency() -> Result<(String, AggOut)> {
    Err(erp_fields::Error::programming(
        "the 'sum_currency' aggregate is not supported until M3",
    ))
}

fn invalid_having(external: bool, item: &Value) -> erp_fields::Error {
    bad(
        external,
        format!("Invalid having clause {item}: it should be a domain-like clause"),
    )
}

fn pop(stack: &mut Vec<String>, external: bool) -> Result<String> {
    stack
        .pop()
        .ok_or_else(|| bad(external, "Invalid having clause: missing operand"))
}

/// Whether a group-by spec may duplicate rows (a many2many, directly or through a many2one path):
/// grouping sets then need care with duplicate-sensitive aggregates (GRP §4.7).
pub(crate) fn might_duplicate_rows(
    registry: &Registry,
    model: &ModelSpec,
    spec: &str,
    external: bool,
) -> Result<bool> {
    let parts = parse_spec_or_err(spec, external)?;
    let field = model.field(parts.field).ok_or_else(|| {
        bad(
            external,
            format!("Invalid field '{}' on model '{}'", parts.field, model.name),
        )
    })?;
    match parts.path {
        Some(path) if matches!(field.ty, FieldType::Many2one { .. }) => {
            let comodel = crate::relational::comodel_of(registry, field)?;
            might_duplicate_rows(registry, comodel, path, external)
        }
        _ => Ok(matches!(field.ty, FieldType::Many2many { .. })),
    }
}

impl Transaction {
    /// Whether PostgreSQL knows the time zone `tz` (`pg_timezone_names`), asked once per zone and
    /// transaction.
    pub(crate) async fn pg_timezone_known(&mut self, tz: &str) -> Result<bool> {
        if let Some(known) = self.engine.pg_timezones.get(tz) {
            return Ok(*known);
        }
        let rows = self
            .query(
                "SELECT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name = $1)",
                &[&tz],
            )
            .await?;
        let known: bool = rows
            .first()
            .ok_or_else(|| erp_fields::Error::programming("EXISTS returned no row"))?
            .try_get(0)
            .map_err(|e| {
                erp_fields::Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not decode a time zone check",
                )
                .with_source(e)
            })?;
        self.engine.pg_timezones.insert(tz.to_owned(), known);
        Ok(known)
    }
}
