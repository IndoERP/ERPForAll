//! Grouped reads (GRP §4.1–4.7): `_read_group`, the public `read_group` and `_read_grouping_sets`.
//!
//! - **One statement.** A grouped read is one `SELECT {group exprs}, {aggregates} FROM ... WHERE
//!   {search predicate} GROUP BY ... HAVING ... ORDER BY ... LIMIT ... OFFSET ...`. The predicate is the one
//!   `search` builds (access domain, active test, relational sub-queries), so access
//!   restrictions are evaluated in SQL; record rules of comodels restrict many2one path joins and
//!   many2many relation joins inside the statement too. A domain known to be empty runs nothing
//!   .
//! - **Flush.** Pending writes of every field the statement reads (domain, group-bys, aggregates,
//!   order, `having`) are flushed first; nothing else is.

mod build;
mod panel;
pub(crate) use panel::panel_counts;
pub(crate) mod decode;
pub(crate) mod spec;
mod value;

use std::any::Any;
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Error, ErrorKind, Result, Value};
use rustc_hash::FxHashMap;

use self::build::{AggTerm, Builder, GroupTerm, Statement, might_duplicate_rows};
pub use self::spec::Granularity;
use self::spec::bad;
pub use self::value::{GroupValue, Groups, RecordsAs};
use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::Records;
use crate::search::{compile_for, flush_read_inputs};

/// ```
/// use erp_domain::Domain;
/// use erp_orm::ReadGroup;
///
/// let args = ReadGroup::new(Domain::TRUE)
///     .groupby(["country_id"])
///     .aggregates(["__count"]);
/// assert_eq!(args.groupby, vec!["country_id"]);
/// assert_eq!(args.aggregates, vec!["__count"]);
/// ```
#[derive(Debug, Clone)]
pub struct ReadGroup {
    /// Records to group.
    pub domain: Domain,
    /// Group-by specs: `field`, `date_field:granularity`, `many2one.path[:granularity]`.
    pub groupby: Vec<String>,
    /// Aggregate specs: `__count`, `field:function`.
    pub aggregates: Vec<String>,
    /// A domain-like list over aggregate specs (`[("value:sum", ">", 8)]` as values).
    pub having: Vec<Value>,
    /// Groups to skip.
    pub offset: u64,
    /// At most this many groups.
    pub limit: Option<u64>,
    /// Order of the groups (group-by and aggregate specs); `None`: the group-by specs.
    pub order: Option<String>,
    /// Also count the groups before `limit`/`offset` ([`Groups::group_count`]), in the same
    /// statement (`COUNT(*) OVER ()`); `web_read_group` uses it for its `length`.
    pub count_groups: bool,
    /// The specs come from client input: malformed specs are `Validation` errors
    /// instead of `Programming` errors.
    pub external: bool,
    /// Check `order` and `having` even when the domain is known to match nothing.
    pub(crate) check_empty: bool,
}

impl ReadGroup {
    /// Group the records matching `domain`.
    pub fn new(domain: Domain) -> Self {
        Self {
            domain,
            groupby: Vec::new(),
            aggregates: Vec::new(),
            having: Vec::new(),
            offset: 0,
            limit: None,
            order: None,
            count_groups: false,
            external: false,
            check_empty: false,
        }
    }

    /// Set the group-by specs.
    #[must_use]
    pub fn groupby<S: Into<String>>(mut self, specs: impl IntoIterator<Item = S>) -> Self {
        self.groupby = specs.into_iter().map(Into::into).collect();
        self
    }

    /// Set the aggregate specs.
    #[must_use]
    pub fn aggregates<S: Into<String>>(mut self, specs: impl IntoIterator<Item = S>) -> Self {
        self.aggregates = specs.into_iter().map(Into::into).collect();
        self
    }

    /// Set the `having` clause (domain-like list items).
    #[must_use]
    pub fn having(mut self, items: Vec<Value>) -> Self {
        self.having = items;
        self
    }

    /// Set `offset`.
    #[must_use]
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Set `limit`.
    #[must_use]
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set `order`.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }

    /// Count the groups before `limit`/`offset` too.
    #[must_use]
    pub fn count_groups(mut self, on: bool) -> Self {
        self.count_groups = on;
        self
    }

    /// Mark the specs as client input.
    #[must_use]
    pub fn external(mut self, on: bool) -> Self {
        self.external = on;
        self
    }

    /// Also check `order` and `having` for a domain that matches nothing (query AST path).
    #[must_use]
    pub(crate) fn check_empty(mut self, on: bool) -> Self {
        self.check_empty = on;
        self
    }
}

/// The arguments of `_read_grouping_sets`.
#[derive(Debug, Clone)]
pub struct ReadGroupingSets {
    /// Records to group.
    pub domain: Domain,
    /// One list of group-by specs per grouping.
    pub grouping_sets: Vec<Vec<String>>,
    /// Aggregate specs, shared by every grouping.
    pub aggregates: Vec<String>,
    /// Order of the groups.
    pub order: Option<String>,
    /// The specs come from client input (see [`ReadGroup::external`]).
    pub external: bool,
}

impl ReadGroupingSets {
    /// Grouping sets over the records matching `domain`.
    pub fn new<S: Into<String>>(
        domain: Domain,
        grouping_sets: impl IntoIterator<Item = Vec<S>>,
    ) -> Self {
        Self {
            domain,
            grouping_sets: grouping_sets
                .into_iter()
                .map(|set| set.into_iter().map(Into::into).collect())
                .collect(),
            aggregates: Vec::new(),
            order: None,
            external: false,
        }
    }

    /// Set the aggregate specs.
    #[must_use]
    pub fn aggregates<S: Into<String>>(mut self, specs: impl IntoIterator<Item = S>) -> Self {
        self.aggregates = specs.into_iter().map(Into::into).collect();
        self
    }

    /// Set `order`.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }

    /// Mark the specs as client input.
    #[must_use]
    pub fn external(mut self, on: bool) -> Self {
        self.external = on;
        self
    }
}

impl Records {
    /// `_read_group`: grouped values and aggregates in one statement, through the method chain
    /// (an addon may override it). Relational group values are one-record recordsets (empty for
    /// the empty group) sharing one prefetch set per column.
    pub async fn _read_group(&self, tx: &mut Transaction, args: ReadGroup) -> Result<Groups> {
        let (groupby, aggregates) = (args.groupby.clone(), args.aggregates.clone());
        let groups = call_method::<(ReadGroup,), Groups>(tx, self, "_read_group", (args,)).await?;
        groups.check(&groupby, &aggregates, "_read_group")?;
        Ok(groups)
    }

    /// The public `read_group` (final): `_read_group` with relational group values as ids
    /// (`False` for the empty group) and `recordset` aggregates as id lists.
    pub async fn read_group(
        &self,
        tx: &mut Transaction,
        args: ReadGroup,
    ) -> Result<Vec<Vec<Value>>> {
        let n = args.groupby.len();
        let groups = self._read_group(tx, args).await?;
        Ok(groups
            .rows()
            .into_iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, v)| v.to_value(if i < n { RecordsAs::Id } else { RecordsAs::Ids }))
                    .collect()
            })
            .collect())
    }

    /// `_read_grouping_sets`: one result per grouping set, each equal to the `_read_group` of that
    /// set, computed in one statement when possible (overridable).
    pub async fn _read_grouping_sets(
        &self,
        tx: &mut Transaction,
        args: ReadGroupingSets,
    ) -> Result<Vec<Groups>> {
        let (sets, aggregates) = (args.grouping_sets.clone(), args.aggregates.clone());
        let results = call_method::<(ReadGroupingSets,), Vec<Groups>>(
            tx,
            self,
            "_read_grouping_sets",
            (args,),
        )
        .await?;
        if results.len() != sets.len() {
            return Err(Error::programming(format!(
                "_read_grouping_sets returned {} results for {} grouping sets",
                results.len(),
                sets.len()
            )));
        }
        for (set, groups) in sets.iter().zip(&results) {
            groups.check(set, &aggregates, "_read_grouping_sets")?;
        }
        Ok(results)
    }
}

/// The empty result of a grouping over no record: no group, or one group of empty values when
/// there is no group-by (PostgreSQL returns one row then).
fn empty_result(
    records: &Records,
    groupby: &[String],
    aggregates: &[String],
    group_terms: &[GroupTerm],
    agg_terms: &[AggTerm],
) -> Groups {
    let env = records.env();
    let (columns, len) = if groupby.is_empty() {
        let columns = agg_terms
            .iter()
            .map(|t| vec![decode::empty_aggregate(env, t.out)])
            .collect();
        (columns, 1)
    } else {
        let width = group_terms.len() + agg_terms.len();
        ((0..width).map(|_| Vec::new()).collect(), 0)
    };
    Groups::from_columns(groupby.into(), aggregates.into(), columns, len)
}

/// A grouped read prepared up to its statement: nothing has been sent to the database yet.
/// `base._read_group` executes it; the query AST's `check` stops here.
pub(crate) enum Plan {
    /// The domain matches nothing: the answer is known and no statement runs.
    Empty(Groups),
    /// The statement to run.
    Statement(Prepared),
}

/// The statement of a grouped read with what decodes its rows.
pub(crate) struct Prepared {
    pub(crate) sql: String,
    pub(crate) params: Vec<erp_sql::param::Param>,
    fields: Vec<(erp_registry::ModelId, erp_registry::FieldId)>,
    groups: Vec<GroupTerm>,
    aggs: Vec<AggTerm>,
    count_groups: bool,
}

/// Everything `base._read_group` does before the statement is sent: the predicate, the group-by
/// and aggregate terms, the field access checks, order and `having`, and the
/// statement text.
pub(crate) async fn plan(
    tx: &mut Transaction,
    records: &Records,
    args: &ReadGroup,
) -> Result<Plan> {
    tx.check_records(records)?;
    if args.groupby.len() > 64 {
        return Err(bad(
            args.external,
            format!(
                "'groupby' accepts at most 64 entries, got {}",
                args.groupby.len()
            ),
        ));
    }
    if args.aggregates.len() > 64 {
        return Err(bad(
            args.external,
            format!(
                "'aggregates' accepts at most 64 entries, got {}",
                args.aggregates.len()
            ),
        ));
    }
    if args.having.len() > 64 {
        return Err(bad(
            args.external,
            format!(
                "'having' accepts at most 64 entries, got {}",
                args.having.len()
            ),
        ));
    }
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let filter = compile_for(tx, records, &args.domain, true).await?;
    let mut b = Builder::new(&registry, env.clone(), spec, args.external, &filter);
    b.validate_companies(tx).await?;
    let mut groups = Vec::with_capacity(args.groupby.len());
    for g in &args.groupby {
        let term = b
            .groupby_term(tx, spec, spec.table.clone(), g.clone(), g)
            .await?;
        groups.push(term);
    }
    for a in &args.aggregates {
        b.prepare_aggregate(tx, a).await?;
    }
    let aggs = args
        .aggregates
        .iter()
        .map(|a| b.aggregate_term(a))
        .collect::<Result<Vec<_>>>()?;
    // group-by and aggregate fields, paths included
    tx.check_read_pairs(&env, &b.touched)?;
    if filter.constant == Some(false) {
        // nothing runs, but the order and `having` are still checked: a stored query that cannot
        // run must not look valid because its domain happens to match nothing today
        if args.check_empty && !groups.is_empty() {
            b.orderby(args.order.as_deref(), &groups)?;
            b.having(&args.having)?;
            tx.check_read_pairs(&env, &b.touched)?;
        }
        return Ok(Plan::Empty(empty_result(
            records,
            &args.groupby,
            &args.aggregates,
            &groups,
            &aggs,
        )));
    }
    if groups.is_empty() && aggs.is_empty() {
        return Err(bad(args.external, "Need groupby or aggregates."));
    }
    let mut order = Vec::new();
    let mut having = None;
    let mut group_by = None;
    if !groups.is_empty() {
        order = b.orderby(args.order.as_deref(), &groups)?;
        having = b.having(&args.having)?;
        group_by = Some(unique(groups.iter().map(GroupTerm::group_item)).join(", "));
    }
    // the comodel order fields of many2one groups were read while ordering
    tx.check_read_pairs(&env, &b.touched)?;
    let count_groups = args.count_groups && !groups.is_empty();
    let sql = b.render(
        &filter,
        &Statement {
            lead: None,
            groups: &groups,
            aggs: &aggs,
            group_by,
            having,
            order,
            count_groups,
            limit: args.limit,
            offset: args.offset,
        },
    )?;
    let params = std::mem::take(&mut b.params);
    let fields = std::mem::take(&mut b.fields);
    Ok(Plan::Statement(Prepared {
        sql,
        params,
        fields,
        groups,
        aggs,
        count_groups,
    }))
}

/// `base._read_group`.
async fn base_read_group(
    tx: &mut Transaction,
    records: &Records,
    args: ReadGroup,
) -> Result<Groups> {
    let prepared = match plan(tx, records, &args).await? {
        Plan::Empty(groups) => return Ok(groups),
        Plan::Statement(prepared) => prepared,
    };
    let env = records.env().clone();
    let Prepared {
        sql,
        params,
        fields,
        groups,
        aggs,
        count_groups,
    } = prepared;
    flush_read_inputs(tx, &fields).await?;
    let width = groups.len() + aggs.len();
    let mut raw: Vec<Vec<GroupValue>> = (0..width).map(|_| Vec::new()).collect();
    let mut total = None;
    let mut rows = 0usize;
    // groups are decoded as the rows arrive (design note `perf-stream.md` §3.1)
    let policy = tx.plan_policy(&params);
    tx.query_each_with(&sql, &params, policy, |_, row| {
        for (i, column) in raw.iter_mut().enumerate() {
            column.push(decode::raw(row, i)?);
        }
        if count_groups && total.is_none() {
            total = match decode::raw(row, width)? {
                GroupValue::Int(n) => u64::try_from(n).ok(),
                _ => None,
            };
        }
        rows += 1;
        Ok(())
    })
    .await?;
    let columns = decode_columns(&env, &groups, &aggs, raw)?;
    let mut result =
        Groups::from_columns(args.groupby.into(), args.aggregates.into(), columns, rows);
    result.set_group_count(total);
    Ok(result)
}

fn decode_columns(
    env: &crate::Env,
    groups: &[GroupTerm],
    aggs: &[AggTerm],
    raw: Vec<Vec<GroupValue>>,
) -> Result<Vec<Vec<GroupValue>>> {
    let mut columns = Vec::with_capacity(groups.len() + aggs.len());
    let mut raw = raw.into_iter();
    for t in groups {
        columns.push(decode::group_column(
            env,
            t.out,
            raw.next().unwrap_or_default(),
            &t.spec,
        )?);
    }
    for a in aggs {
        columns.push(decode::aggregate_column(
            env,
            a.out,
            raw.next().unwrap_or_default(),
        ));
    }
    Ok(columns)
}

/// The aggregates a duplicated row (many2many join) does not change.
fn duplicate_safe(aggregate: &str) -> bool {
    aggregate == "__count"
        || [
            ":max",
            ":min",
            ":bool_and",
            ":bool_or",
            ":array_agg_distinct",
            ":recordset",
            ":count_distinct",
        ]
        .iter()
        .any(|suffix| aggregate.ends_with(suffix))
}

fn unique<T: PartialEq + Clone>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Grouping sets computed together: the many2many specs they contain (by index), their positions in
/// the request and the sets.
type SetBatch = (Vec<usize>, Vec<usize>, Vec<Vec<String>>);

/// Most distinct group-by specs of one grouping-sets statement: `GROUPING()` returns an `int4`
/// bitmask, one bit per expression.
const MAX_GROUPING_SPECS: usize = 31;

/// `base._read_grouping_sets`.
fn base_read_grouping_sets<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: ReadGroupingSets,
) -> BoxFuture<'a, Result<Vec<Groups>>> {
    Box::pin(async move {
        tx.check_records(records)?;
        if args.grouping_sets.is_empty() {
            return Err(bad(
                args.external,
                "The 'grouping_sets' parameter cannot be empty.",
            ));
        }
        if args.grouping_sets.len() > 64 {
            return Err(bad(
                args.external,
                format!(
                    "'grouping_sets' accepts at most 64 sets, got {}",
                    args.grouping_sets.len()
                ),
            ));
        }
        if args.aggregates.len() > 64 {
            return Err(bad(
                args.external,
                format!(
                    "'aggregates' accepts at most 64 entries, got {}",
                    args.aggregates.len()
                ),
            ));
        }
        let env = records.env().clone();
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(records.model_id());
        let all_specs: Vec<String> = unique(args.grouping_sets.iter().flatten().cloned());
        if all_specs.len() > MAX_GROUPING_SPECS {
            return Err(bad(
                args.external,
                format!(
                    "Grouping sets accept at most {MAX_GROUPING_SPECS} distinct groupby specs, got {}",
                    all_specs.len()
                ),
            ));
        }
        let mut aggregates = args.aggregates.clone();
        let mut order = args.order.clone();
        let mut m2m_specs: Vec<String> = Vec::new();
        if args.grouping_sets.len() > 1 {
            for s in &all_specs {
                if might_duplicate_rows(&registry, spec, s, args.external)? {
                    m2m_specs.push(s.clone());
                }
            }
        }
        if !m2m_specs.is_empty() && aggregates.iter().any(|a| !duplicate_safe(a)) {
            // Split the sets by the many2many group-bys they contain (GRP §4.7).
            let mut batches: Vec<SetBatch> = Vec::new();
            for (i, set) in args.grouping_sets.iter().enumerate() {
                let key: Vec<usize> = (0..m2m_specs.len())
                    .filter(|k| set.contains(&m2m_specs[*k]))
                    .collect();
                match batches.iter_mut().find(|(k, _, _)| *k == key) {
                    Some((_, indexes, sets)) => {
                        indexes.push(i);
                        sets.push(set.clone());
                    }
                    None => batches.push((key, vec![i], vec![set.clone()])),
                }
            }
            batches.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
            if batches.len() > 1 {
                let mut result: Vec<Option<Groups>> = vec![None; args.grouping_sets.len()];
                for (_, indexes, sets) in batches {
                    let in_batch: Vec<&String> = sets.iter().flatten().collect();
                    let sub_order: Vec<String> = order
                        .as_deref()
                        .unwrap_or_default()
                        .split(',')
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                        .filter(|part| {
                            !all_specs
                                .iter()
                                .filter(|s| !in_batch.contains(s))
                                .any(|s| part.starts_with(s.as_str()))
                        })
                        .map(str::to_owned)
                        .collect();
                    let sub = ReadGroupingSets {
                        domain: args.domain.clone(),
                        grouping_sets: sets,
                        aggregates: args.aggregates.clone(),
                        order: (!sub_order.is_empty()).then(|| sub_order.join(",")),
                        external: args.external,
                    };
                    let sub_results = records._read_grouping_sets(tx, sub).await?;
                    for (index, groups) in indexes.into_iter().zip(sub_results) {
                        if let Some(slot) = result.get_mut(index) {
                            *slot = Some(groups);
                        }
                    }
                }
                return result
                    .into_iter()
                    .map(|g| g.ok_or_else(|| Error::programming("a grouping set got no result")))
                    .collect();
            }
        } else if !m2m_specs.is_empty() && aggregates.iter().any(|a| a == "__count") {
            // `__count` over duplicated rows: count distinct ids instead, in the same statement
            for a in &mut aggregates {
                if a == "__count" {
                    *a = "id:count_distinct".to_owned();
                }
            }
            order = order.map(|o| o.replace("__count", "id:count_distinct"));
        }
        grouping_sets_query(
            tx,
            records,
            &args,
            &all_specs,
            &aggregates,
            order.as_deref(),
        )
        .await
    })
}

async fn grouping_sets_query(
    tx: &mut Transaction,
    records: &Records,
    args: &ReadGroupingSets,
    all_specs: &[String],
    aggregates: &[String],
    order: Option<&str>,
) -> Result<Vec<Groups>> {
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let filter = compile_for(tx, records, &args.domain, true).await?;
    let mut b = Builder::new(&registry, env.clone(), spec, args.external, &filter);
    b.validate_companies(tx).await?;
    let mut terms = Vec::with_capacity(all_specs.len());
    for s in all_specs {
        terms.push(
            b.groupby_term(tx, spec, spec.table.clone(), s.clone(), s)
                .await?,
        );
    }
    for a in aggregates {
        b.prepare_aggregate(tx, a).await?;
    }
    let aggs = aggregates
        .iter()
        .map(|a| b.aggregate_term(a))
        .collect::<Result<Vec<_>>>()?;
    tx.check_read_pairs(&env, &b.touched)?;
    let empty = |set: &Vec<String>| {
        Groups::from_columns(
            set.clone().into(),
            args.aggregates.clone().into(),
            (0..set.len() + aggs.len()).map(|_| Vec::new()).collect(),
            0,
        )
    };
    if filter.constant == Some(false) {
        return Ok(args.grouping_sets.iter().map(empty).collect());
    }
    // one GROUPING() bit per distinct expression, the rightmost expression being the lowest bit
    let items: Vec<String> = unique(terms.iter().map(GroupTerm::group_item));
    let grouping = if items.is_empty() {
        "0".to_owned()
    } else {
        format!("GROUPING({})", items.join(", "))
    };
    let order_items = b.orderby(order, &terms)?;
    tx.check_read_pairs(&env, &b.touched)?;
    let item_of: FxHashMap<&str, String> = terms
        .iter()
        .map(|t| (t.spec.as_str(), t.group_item()))
        .collect();
    let sets_sql: Vec<String> = unique(args.grouping_sets.iter().map(|set| {
        let set_items: Vec<String> =
            unique(set.iter().filter_map(|s| item_of.get(s.as_str()).cloned()));
        format!("({})", set_items.join(", "))
    }));
    let sql = b.render(
        &filter,
        &Statement {
            lead: Some(grouping),
            groups: &terms,
            aggs: &aggs,
            group_by: Some(format!("GROUPING SETS ({})", sets_sql.join(", "))),
            having: None,
            order: order_items,
            count_groups: false,
            limit: None,
            offset: 0,
        },
    )?;
    let params = std::mem::take(&mut b.params);
    let fields = std::mem::take(&mut b.fields);
    flush_read_inputs(tx, &fields).await?;
    let width = terms.len() + aggs.len();
    let mut masks = Vec::new();
    let mut raw: Vec<Vec<GroupValue>> = (0..width).map(|_| Vec::new()).collect();
    // groups are decoded as the rows arrive (design note `perf-stream.md` §3.1)
    let policy = tx.plan_policy(&params);
    tx.query_each_with(&sql, &params, policy, |_, row| {
        masks.push(match decode::raw(row, 0)? {
            GroupValue::Int(m) => m,
            other => {
                return Err(Error::new(
                    ErrorKind::Database,
                    format!("GROUPING() returned {other:?}"),
                ));
            }
        });
        for (i, column) in raw.iter_mut().enumerate() {
            column.push(decode::raw(row, i + 1)?);
        }
        Ok(())
    })
    .await?;
    let rows = masks.len();
    let columns = decode_columns(&env, &terms, &aggs, raw)?;
    let all = Groups::from_columns(
        all_specs.to_vec().into(),
        args.aggregates.clone().into(),
        columns,
        rows,
    );
    let bit_of: FxHashMap<&str, i64> = items
        .iter()
        .rev()
        .enumerate()
        .map(|(i, item)| (item.as_str(), 1i64 << i))
        .collect();
    let all_bits: i64 = bit_of.values().sum();
    let agg_columns: Vec<usize> = (terms.len()..width).collect();
    let mut out = Vec::with_capacity(args.grouping_sets.len());
    for set in &args.grouping_sets {
        let present: i64 = unique(set.iter().filter_map(|s| item_of.get(s.as_str())))
            .iter()
            .filter_map(|item| bit_of.get(item.as_str()))
            .sum();
        let mask = all_bits - present;
        let rows: Vec<usize> = masks
            .iter()
            .enumerate()
            .filter(|(_, m)| **m == mask)
            .map(|(i, _)| i)
            .collect();
        let mut wanted: Vec<usize> = set
            .iter()
            .filter_map(|s| all_specs.iter().position(|a| a == s))
            .collect();
        wanted.extend(agg_columns.iter().copied());
        let columns = all.take_rows(&rows, &wanted);
        out.push(Groups::from_columns(
            set.clone().into(),
            args.aggregates.clone().into(),
            columns,
            rows.len(),
        ));
    }
    Ok(out)
}

// ----- base implementations ----------------------------------------------------------------------

type ReadGroupFn = MethodFn<(ReadGroup,), Groups>;
type ReadGroupingSetsFn = MethodFn<(ReadGroupingSets,), Vec<Groups>>;

fn read_group_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (ReadGroup,),
    _sup: Super<(ReadGroup,), Groups>,
) -> BoxFuture<'a, Result<Groups>> {
    Box::pin(base_read_group(tx, records, args.0))
}

fn read_grouping_sets_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (ReadGroupingSets,),
    _sup: Super<(ReadGroupingSets,), Vec<Groups>>,
) -> BoxFuture<'a, Result<Vec<Groups>>> {
    base_read_grouping_sets(tx, records, args.0)
}

fn read_group_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ReadGroupFn = read_group_impl;
    &F
}

fn read_grouping_sets_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ReadGroupingSetsFn = read_grouping_sets_impl;
    &F
}

/// `base._read_group` and `base._read_grouping_sets`, registered by [`crate::Dispatch::build`]
/// like the CRUD built-ins.
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_READ_GROUP, &BASE_READ_GROUPING_SETS];

static BASE_READ_GROUP: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "_read_group",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ReadGroupFn>,
    entry: read_group_entry,
    file: file!(),
    line: line!(),
};

static BASE_READ_GROUPING_SETS: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "_read_grouping_sets",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ReadGroupingSetsFn>,
    entry: read_grouping_sets_entry,
    file: file!(),
    line: line!(),
};
