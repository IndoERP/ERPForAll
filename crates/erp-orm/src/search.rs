//! A search runs these steps, all in SQL (no record is ever filtered in memory):
//!
//! 1. Access: outside superuser mode the model check of the [`crate::AccessPolicy`] runs, then its
//!    access domain is ANDed into the search domain *before optimization*. A `FALSE` access domain
//!    gives an empty result without a query.
//! 2. Active test: when the model has an active field, the context does not set
//!    `active_test=False`, and the caller's domain has no condition on that field, `active in
//!    {True}` is ANDed in front.
//! 3. Hierarchy conditions are resolved to ids ([`crate::hierarchy`]); the combined domain is
//!    optimized to FULL ([`crate::optimize`]), its relational sub-domains are completed
//!    ([`crate::prepare`]) and it is planned (`erp_sql::filter::plan_where`): the plan holds the
//!    literal-free shape of the predicate (joins and sub-queries included) and the parameters. A
//!    `FALSE` domain runs no query.
//! 4. The pending writes of the fields the predicate and the order read are flushed — never the
//!    fetched columns themselves. Reading an audit field
//!    (`write_date`, `write_uid`, …) flushes the whole model, since every pending write changes them.
//! 5. One `SELECT "t"."id"[, columns…] … ORDER BY … LIMIT $n OFFSET $m` fetches ids and the
//!    requested columns together; values go into the cache without overwriting pending writes.
//!
//! # Memoization
//!
//! [`SearchMemo`] keeps, per ORM, the SQL text of each predicate *shape*
//! ([`erp_sql::filter::ShapeKey`]: field ids, operators, NULL decisions and parameter types, never
//! literal values) and each compiled order specification. Optimization and planning run on every
//! call, so relative dates, record rules and context are always current and nothing value-dependent
//! is ever reused; a hit skips rendering. Memory is bounded by a two-generation eviction: entries
//! used since the last rotation survive it.
//!
//! # Plans
//!
//! Statements whose only parameters are ids,
//! `LIMIT`/`OFFSET` and DML arrays stay named prepared statements cached per connection, where
//! the generic plan is as good and saves planning ([`PlanPolicy`]).

use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::graph::ModelGraph;
use erp_domain::{Domain, DomainKind, Operator, OptimizationLevel};
use erp_fields::{
    ColumnType, DateTime, Error, ErrorKind, FieldType, RealId, RecId, Result, Storage, Value,
};
use erp_registry::{FieldId, ModelId, ModelSpec};
use erp_sql::filter::{
    ShapeKey, UNACCENT_WRAPPER, Unaccent, WhereContext, plan_where, render_where_at,
};

/// Joins of a statement as `(alias, text)`; texts of equal aliases are equal by construction.
pub(crate) type JoinList = Arc<[(String, String)]>;

/// The join texts of several lists, each alias once, in first-seen order (the predicate's and the
/// order's joins follow the same aliasing rule, so a shared path is joined once).
pub(crate) fn merge_joins(lists: &[&[(String, String)]]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = String::new();
    for list in lists {
        for (alias, text) in *list {
            if !seen.contains(&alias.as_str()) {
                seen.push(alias);
                out.push_str(text);
            }
        }
    }
    out
}

/// # Errors
///
/// `Programming` naming the alias and both joins.
pub(crate) fn merge_joins_checked(lists: &[&[(String, String)]]) -> Result<String> {
    erp_sql::order::check_join_conflicts(lists)?;
    Ok(merge_joins(lists))
}

/// The memoized text of a predicate shape.
#[derive(Debug, Clone)]
pub(crate) struct MemoText {
    sql: Arc<str>,
    joins: JoinList,
}
use erp_sql::order::{OrderBy, compile_order};
use erp_sql::param::{Param, Scalar, ScalarKind};
use erp_sql::select::{SearchSql, count, select_cached, select_ids_where_cached};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use crate::Transaction;
use crate::access::Operation;
use crate::cache::{Cache, ModelWriter};
use crate::env::Env;
use crate::field_access::FieldOp;
use crate::optimize::{OptimizeCtx, ZoneTz, now_utc, optimize_domain, primitive_now};
use crate::records::{Ids, Model, RecordSet, Records};
use crate::transaction::raw_row::{CellSource, RawRow};
use crate::transaction::{Args, PipelinedQuery};

pub const NAME_SEARCH_LIMIT: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Fetch {
    /// Only the ids (`search`).
    #[default]
    Ids,
    /// Every prefetchable stored field (`search_fetch(domain)` with no field names).
    Prefetchable,
    /// These fields (`display_name` fetches its rec name).
    Fields(Vec<String>),
}

/// Internal capability request; validated against the caller's schema and policy before SQL.
pub(crate) struct CatalogRequest<'a> {
    pub(crate) domain: &'a Domain,
    pub(crate) fields: [&'a str; 2],
    pub(crate) order: &'a str,
    pub(crate) pairs: usize,
    pub(crate) bytes: usize,
    pub(crate) charge: &'a mut (dyn FnMut(usize) -> Result<()> + Send),
}

/// Options of a search: pagination, order, active test, columns to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    /// Records to skip (`offset`, default 0).
    pub offset: u64,
    /// At most this many records (`None`: all).
    pub limit: Option<u64>,
    pub order: Option<String>,
    pub active_test: bool,
    /// Columns fetched with the ids.
    pub fetch: Fetch,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: None,
            order: None,
            active_test: true,
            fetch: Fetch::Ids,
        }
    }
}

impl SearchOptions {
    /// Default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `limit`.
    #[must_use]
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set `offset`.
    #[must_use]
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Set `order`.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }

    /// Set `active_test`.
    #[must_use]
    pub fn active_test(mut self, on: bool) -> Self {
        self.active_test = on;
        self
    }

    /// Fetch these fields with the ids.
    #[must_use]
    pub fn fetch_fields<S: Into<String>>(mut self, fields: impl IntoIterator<Item = S>) -> Self {
        self.fetch = Fetch::Fields(fields.into_iter().map(Into::into).collect());
        self
    }

    /// Fetch every prefetchable stored field with the ids.
    #[must_use]
    pub fn fetch_prefetchable(mut self) -> Self {
        self.fetch = Fetch::Prefetchable;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameSearch {
    /// Further restriction.
    pub domain: Domain,
    /// How `display_name` is compared with the name.
    pub operator: Operator,
    /// At most this many results (`None`: all).
    pub limit: Option<u64>,
}

impl Default for NameSearch {
    fn default() -> Self {
        Self {
            domain: Domain::TRUE,
            operator: Operator::ILike,
            limit: Some(NAME_SEARCH_LIMIT),
        }
    }
}

impl NameSearch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the restricting domain.
    #[must_use]
    pub fn domain(mut self, domain: Domain) -> Self {
        self.domain = domain;
        self
    }

    /// Set the operator.
    #[must_use]
    pub fn operator(mut self, operator: Operator) -> Self {
        self.operator = operator;
        self
    }

    /// Set the limit (`None`: all).
    #[must_use]
    pub fn limit(mut self, limit: Option<u64>) -> Self {
        self.limit = limit;
        self
    }
}

// ----- memo -----------------------------------------------------------------------------------

/// A bounded map with two generations: inserts go to the young one; when it holds half the
/// capacity it becomes the old one and the previous old generation is dropped. A hit in the old
/// generation moves the entry back to the young one, so entries in use survive rotations. At most
/// `capacity` entries are kept; capacity 0 disables storing.
pub(crate) struct TwoGen<K, V> {
    half: usize,
    young: FxHashMap<K, V>,
    old: FxHashMap<K, V>,
}

impl<K: Hash + Eq, V: Clone> TwoGen<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            half: capacity / 2,
            young: FxHashMap::default(),
            old: FxHashMap::default(),
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.young.get(key) {
            return Some(value.clone());
        }
        let (key, value) = self.old.remove_entry(key)?;
        self.insert(key, value.clone());
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.half == 0 {
            return;
        }
        if self.young.len() >= self.half {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(key, value);
    }

    pub(crate) fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
}

/// Counters of the search memo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoStats {
    /// Predicates whose text came from the memo.
    pub hits: u64,
    /// Predicates rendered (and stored).
    pub misses: u64,
    /// Predicate texts currently stored.
    pub entries: usize,
}

/// Compiled orders per (model, order specification, language).
type OrderMemo = TwoGen<(ModelId, Box<str>, Option<Arc<str>>), Arc<OrderBy>>;

/// Predicate texts per (shape, placeholder offset) and compiled orders, per ORM; plus the
/// detected `unaccent` status.
pub(crate) struct SearchMemo {
    filters: Mutex<TwoGen<(ShapeKey, usize), MemoText>>,
    orders: Mutex<OrderMemo>,
    hits: AtomicU64,
    misses: AtomicU64,
    /// 0 unknown, 1 off, 2 present, 3 indexable.
    unaccent: AtomicU8,
}

impl SearchMemo {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            filters: Mutex::new(TwoGen::new(capacity)),
            orders: Mutex::new(TwoGen::new(capacity)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            unaccent: AtomicU8::new(0),
        }
    }

    /// The text of a shape with placeholders after `offset`, rendered on a miss.
    fn text(
        &self,
        key: &(ShapeKey, usize),
        render: impl FnOnce() -> erp_sql::filter::Rendered,
    ) -> MemoText {
        let found = self
            .filters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key);
        if let Some(text) = found {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return text;
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let rendered = render();
        let text = MemoText {
            sql: rendered.sql.into(),
            joins: rendered.joins.into(),
        };
        self.filters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), text.clone());
        text
    }

    fn order(
        &self,
        model: ModelId,
        spec: &str,
        lang: Option<&str>,
        compile: impl FnOnce() -> Result<OrderBy>,
    ) -> Result<Arc<OrderBy>> {
        let key = (model, Box::<str>::from(spec), lang.map(Arc::from));
        if let Some(found) = self
            .orders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(found);
        }
        let compiled = Arc::new(compile()?);
        self.orders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Arc::clone(&compiled));
        Ok(compiled)
    }

    fn unaccent(&self) -> Option<Unaccent> {
        match self.unaccent.load(Ordering::Acquire) {
            1 => Some(Unaccent::Off),
            2 => Some(Unaccent::Present),
            3 => Some(Unaccent::Indexable),
            _ => None,
        }
    }

    pub(crate) fn set_unaccent(&self, status: Unaccent) {
        let code = match status {
            Unaccent::Off => 1,
            Unaccent::Present => 2,
            Unaccent::Indexable => 3,
        };
        self.unaccent.store(code, Ordering::Release);
    }

    pub(crate) fn stats(&self) -> MemoStats {
        MemoStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: self
                .filters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
        }
    }
}

// ----- unaccent -----------------------------------------------------------------------------------

fn unaccent_missing() -> Error {
    Error::programming(
        "OrmConfig.unaccent is on but the database has no unaccent() function: install the \
         PostgreSQL extension (CREATE EXTENSION unaccent) or turn the option off",
    )
}

impl Transaction {
    /// The accent folding searches use: `Off` unless [`crate::OrmConfig::unaccent`] is on; then
    /// `Indexable` when the immutable wrapper exists (created by schema sync), `Present` when only
    /// `unaccent()` does, and an error when neither does. Detected once per ORM.
    pub(crate) async fn unaccent_status(&mut self) -> Result<Unaccent> {
        if !self.config().unaccent {
            return Ok(Unaccent::Off);
        }
        if let Some(status) = self.shared.search_memo.unaccent() {
            return Ok(status);
        }
        let sql = "SELECT p.proname FROM pg_proc p WHERE p.proname = ANY($1) AND p.pronargs = 1 \
                   AND pg_function_is_visible(p.oid)";
        let names = vec![UNACCENT_WRAPPER.to_owned(), "unaccent".to_owned()];
        let rows = self.query(sql, &[&names]).await?;
        let found: Vec<String> = rows
            .iter()
            .filter_map(|r| r.try_get::<_, String>(0).ok())
            .collect();
        let status = if found.iter().any(|n| n == UNACCENT_WRAPPER) {
            Unaccent::Indexable
        } else if found.iter().any(|n| n == "unaccent") {
            Unaccent::Present
        } else {
            return Err(unaccent_missing());
        };
        self.shared.search_memo.set_unaccent(status);
        Ok(status)
    }

    /// Fails when the function is missing.
    pub(crate) async fn setup_unaccent(&mut self) -> Result<Unaccent> {
        if !self.config().unaccent {
            return Ok(Unaccent::Off);
        }
        let rows = self
            .query(
                "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = \
                 e.extnamespace WHERE e.extname = 'unaccent'",
                &[],
            )
            .await?;
        let status = match rows.first() {
            Some(row) => {
                let schema: String = row.try_get(0).map_err(|e| {
                    Error::new(ErrorKind::Database, "could not decode a schema name").with_source(e)
                })?;
                self.batch(&erp_sql::ddl::create_unaccent_wrapper(&schema))
                    .await?;
                Unaccent::Indexable
            }
            None => {
                let rows = self
                    .query(
                        "SELECT 1 FROM pg_proc p WHERE p.proname = 'unaccent' AND p.pronargs = 1 \
                         AND pg_function_is_visible(p.oid)",
                        &[],
                    )
                    .await?;
                if rows.is_empty() {
                    return Err(unaccent_missing());
                }
                Unaccent::Present
            }
        };
        self.shared.search_memo.set_unaccent(status);
        Ok(status)
    }
}

// ----- compilation ------------------------------------------------------------------------------

/// A planned predicate with its (memoized) text.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) sql: Arc<str>,
    /// Joins of the main table the predicate needs.
    pub(crate) join_list: JoinList,
    pub(crate) params: Vec<Param>,
    pub(crate) fields: Vec<(ModelId, FieldId)>,
    pub(crate) constant: Option<bool>,
    /// The predicate reads other rows of its own model's table (a join, a sub-select, a
    /// hierarchy walk or a user path through it): a create cannot fuse it into its INSERT.
    pub(crate) reads_own_rows: bool,
    /// A hierarchy start set of the domain was found by a search (names, many2many ids): what the
    /// predicate lets through depends on data it does not list, so a decision made with it is
    /// never remembered.
    pub(crate) volatile: bool,
}

fn false_compiled() -> Arc<Compiled> {
    Arc::new(Compiled {
        sql: Arc::from("FALSE"),
        join_list: Arc::from([]),
        params: Vec::new(),
        fields: Vec::new(),
        constant: Some(false),
        reads_own_rows: false,
        volatile: false,
    })
}

/// Optimize `domain` to FULL for `records`' model and environment and plan it; the text comes from
/// the memo.
async fn compile_domain(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    unaccent: Unaccent,
) -> Result<Arc<Compiled>> {
    compile_domain_at(tx, records, domain, unaccent, 0).await
}

fn prepare_sql_domain(tx: &Transaction, records: &Records, domain: &Domain) -> Result<Domain> {
    let env = records.env();
    tx.validate_cached_search_language(env)?;
    let spec = records.spec();
    tx.check_generated_query_domain(env, spec, domain)?;
    let tz = ZoneTz::new(env.tz());
    let now = now_utc()?;
    {
        let lookup = crate::access::TxAccess::new(tx, env);
        let optimized = optimize_domain(
            &OptimizeCtx {
                registry: env.registry(),
                tz: &tz,
                now,
                su: env.su(),
                access: Some(&lookup),
            },
            spec,
            domain,
            OptimizationLevel::Full,
        )?;
        crate::prepare::prepare(
            &crate::prepare::PrepareCtx {
                tx,
                env,
                tz: &tz,
                now,
                access: &lookup,
            },
            spec,
            &optimized,
        )
    }
}

/// [`compile_domain`] with placeholders numbered after `offset` (a sub-query of a statement that
/// binds `offset` parameters before it); only `offset == 0` texts go through the memo.
async fn compile_domain_at(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    unaccent: Unaccent,
    offset: usize,
) -> Result<Arc<Compiled>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let env = records.env();
    let spec = records.spec();
    let prepared = prepare_sql_domain(tx, records, domain)?;
    let registry = env.registry();
    let context = WhereContext {
        registry,
        model: spec,
        unaccent,
        lang: env.lang(),
    };
    let fields = erp_sql::filter::where_fields(&prepared, &context)?;
    let company = tx.company_query_values(env, &fields).await?;
    let mut plan = match &company {
        Some(values) => {
            erp_sql::filter::plan_where_with_company(&prepared, &context, values, offset + 1)?
        }
        None => plan_where(&prepared, &context)?,
    };
    // Ordinary record-rule fields retain their existing privileged predicate semantics.
    // Database expressions may not declassify protected sources.
    for (model, field) in &plan.fields {
        if tx.shared.graph.generated_read((*model, *field)).is_some() {
            tx.check_query_field(env, *model, *field)?;
        }
    }
    // principal parameters take this environment's values; the text stays principal-free
    tx.bind_principal_params(env, &mut plan.params)?;
    let text = tx.search_memo_text(&plan.key, offset, || {
        render_where_at(&plan.key, registry, offset + usize::from(company.is_some()))
    });
    let reads_own_rows = plan.key.selects_from(spec.id);
    Ok(Arc::new(Compiled {
        sql: text.sql,
        join_list: text.joins,
        params: {
            let mut params = Vec::new();
            if let Some(values) = &company {
                params.push(values.param()?);
            }
            params.extend(plan.params);
            params
        },
        fields: plan.fields,
        constant: plan.constant,
        reads_own_rows,
        volatile: false,
    }))
}

// Bind the original request before callback field checks. Returned predicates then
// enter the same access preparation and hierarchy stages as caller predicates.
async fn expand_user_search(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
) -> Result<Domain> {
    let env = records.env();
    tx.prepare_access_for_domain(env, domain).await?;
    if !env.su() {
        tx.prepare_access(env).await?;
        tx.check_model(env, records.model_id(), Operation::Read)?;
    }
    tx.check_domain_fields(env, records.spec(), domain)?;
    crate::search_method::expand_search_domains(tx, records, domain)
}

/// The access predicate of a search on `records`' model (design note §3.3), its hierarchy
/// conditions resolved: `None` when it restricts nothing (superuser mode, a `TRUE` predicate).
/// The model check runs first: a refused model is an `Access` error before any SQL; then the
/// groups of the fields `domain` names.
async fn search_access(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
) -> Result<Option<Domain>> {
    let env = records.env();
    tx.validate_search_language(env).await?;
    // `access` conditions of the search domain resolve against the non-superuser environment,
    // also under `sudo()`: its subject and the policy must be current first
    tx.prepare_access_for_domain(env, domain).await?;
    // refused companies stop a search in superuser mode too
    if env.companies()?.is_empty() {
        // an environment that names no companies (`with_user(..).sudo()`) binds the subject's
        // default companies in principal parameters: resolve the subject first (review FV2)
        tx.prepare_access(env).await?;
    }
    if env.su() {
        return Ok(None);
    }
    tx.prepare_access(env).await?;
    tx.check_model(env, records.model_id(), Operation::Read)?;
    // the fields the caller names, not the ones the record rules read
    tx.check_domain_fields(env, records.spec(), domain)?;
    let access = tx.access_predicate(env, records.model_id(), Operation::Read)?;
    if access.is_true() {
        return Ok(None);
    }
    if access.is_false() {
        return Ok(Some(Domain::FALSE));
    }
    let security = predicate_records(records);
    let access = crate::search_method::expand_policy(tx, security.env(), security.spec(), &access)?;
    tx.prepare_access_for_domain(security.env(), &access)
        .await?;
    crate::hierarchy::resolve(tx, &security.engine_sudo(), &access, true)
        .await
        .map(Some)
}

/// Steps 1–3 of a search (see the module documentation).
async fn compile_search(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    access: Option<&Domain>,
    active_test: bool,
    unaccent: Unaccent,
) -> Result<Arc<Compiled>> {
    compile_search_at(tx, records, domain, access, active_test, unaccent, 0).await
}

async fn compile_search_at(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    access: Option<&Domain>,
    active_test: bool,
    unaccent: Unaccent,
    offset: usize,
) -> Result<Arc<Compiled>> {
    tx.admit_records(records).await?;
    if access.is_some_and(Domain::is_false) {
        return Ok(false_compiled());
    }
    let env = records.env();
    let spec = records.spec();
    let mut domain = domain.clone();
    if let Some(active) = spec.active_name.map(|f| spec.field_by_id(f).name)
        && active_test
        && env.context().active_test()
        && !domain
            .iter_conditions()
            .filter_map(Domain::as_condition)
            .any(|c| c.field_expr() == active)
    {
        let condition = Domain::condition(active, Operator::In, erp_domain::Value::set([true]))?;
        domain = condition & domain;
    }
    let user = compile_domain_at(tx, records, &domain, unaccent, offset).await?;
    let Some(access) = access else {
        return Ok(user);
    };
    if user.constant == Some(false) {
        return Ok(user);
    }
    let security = compile_domain_at(
        tx,
        &predicate_records(records),
        access,
        unaccent,
        offset + user.params.len(),
    )
    .await?;
    and_compiled(&user, &security)
}

/// Two compiled predicates ANDed (`b`'s placeholders numbered after `a`'s).
fn and_compiled(a: &Arc<Compiled>, b: &Arc<Compiled>) -> Result<Arc<Compiled>> {
    Ok(match (a.constant, b.constant) {
        (Some(false), _) | (_, Some(false)) => false_compiled(),
        (Some(true), _) => Arc::clone(b),
        (_, Some(true)) => Arc::clone(a),
        (None, None) => {
            erp_sql::order::check_join_conflicts(&[&a.join_list, &b.join_list])?;
            let mut joins: Vec<(String, String)> = a.join_list.to_vec();
            for join in b.join_list.iter() {
                if !joins.iter().any(|(alias, _)| *alias == join.0) {
                    joins.push(join.clone());
                }
            }
            let mut fields = a.fields.clone();
            fields.extend(b.fields.iter().filter(|f| !a.fields.contains(f)));
            let mut params = a.params.clone();
            params.extend(b.params.iter().cloned());
            Arc::new(Compiled {
                sql: Arc::from(format!("{} AND {}", a.sql, b.sql)),
                join_list: joins.into(),
                params,
                fields,
                constant: None,
                reads_own_rows: a.reads_own_rows || b.reads_own_rows,
                volatile: a.volatile || b.volatile,
            })
        }
    })
}

/// The full predicate of a search on `records`' model (access domain, active test when
/// `active_test`, hierarchy resolution, FULL optimization, completed relational sub-domains).
pub(crate) async fn compile_for(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    active_test: bool,
) -> Result<Arc<Compiled>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let domain = expand_user_search(tx, records, domain).await?;
    let access = search_access(tx, records, &domain).await?;
    let domain = crate::hierarchy::resolve(tx, records, &domain, true).await?;
    let unaccent = tx.unaccent_status().await?;
    compile_search(tx, records, &domain, access.as_ref(), active_test, unaccent).await
}

/// [`compile_for`] for a sub-query embedded in a statement that already binds `offset`
/// parameters: placeholders start at `$offset+1` (the text is not memoized).
pub(crate) async fn compile_for_at(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    active_test: bool,
    offset: usize,
) -> Result<Arc<Compiled>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let domain = expand_user_search(tx, records, domain).await?;
    let access = search_access(tx, records, &domain).await?;
    let domain = crate::hierarchy::resolve(tx, records, &domain, true).await?;
    let unaccent = tx.unaccent_status().await?;
    compile_search_at(
        tx,
        records,
        &domain,
        access.as_ref(),
        active_test,
        unaccent,
        offset,
    )
    .await
}

/// The access predicate of `records`' model for its environment as a sub-query predicate with
/// placeholders after `offset` (`None` when it restricts nothing, see [`read_restriction`]).
pub(crate) async fn read_restriction_at(
    tx: &mut Transaction,
    records: &Records,
    offset: usize,
) -> Result<Option<Arc<Compiled>>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let env = records.env();
    env.companies()?;
    if env.su() {
        return Ok(None);
    }
    tx.prepare_access(env).await?;
    tx.check_model(env, records.model_id(), Operation::Read)?;
    let access = tx.access_predicate(env, records.model_id(), Operation::Read)?;
    if access.is_true() {
        return Ok(None);
    }
    if access.is_false() {
        return Ok(Some(false_compiled()));
    }
    let records = &predicate_records(records);
    let security = records.clone();
    let access = crate::search_method::expand_policy(tx, security.env(), security.spec(), &access)?;
    tx.prepare_access_for_domain(security.env(), &access)
        .await?;
    let access = crate::hierarchy::resolve(tx, &security.engine_sudo(), &access, true).await?;
    let unaccent = tx.unaccent_status().await?;
    compile_domain_at(tx, records, &access, unaccent, offset)
        .await
        .map(Some)
}

/// `records` in an environment whose context is the one every access predicate is compiled
/// under: `active_test` off and no time zone (UTC), whatever the caller's context says. The
/// predicate is a function of the binding only (design note: "`active_test` is off
/// inside the predicate"), which is what lets the record memos key their entries by the binding
/// alone. `allowed_company_ids` is kept (rules do not read it).
pub(crate) fn predicate_records(records: &Records) -> Records {
    let mut context = crate::context::Context::new();
    context.set("active_test", erp_fields::Value::Bool(false));
    records.with_env(records.env().with_context_replaced(context))
}

/// An access `predicate` (as [`Transaction::access_predicate`] gives it) compiled for
/// `records`' model and environment: hierarchy conditions resolved, FULL-optimized, planned and
/// bound.
pub(crate) async fn compile_predicate(
    tx: &mut Transaction,
    records: &Records,
    predicate: &Domain,
) -> Result<Arc<Compiled>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let records = &predicate_records(records);
    let security = records.clone();
    let predicate =
        crate::search_method::expand_policy(tx, security.env(), security.spec(), predicate)?;
    tx.prepare_access_for_domain(security.env(), &predicate)
        .await?;
    let (resolved, searched) =
        crate::hierarchy::resolve_tracking(tx, &security.engine_sudo(), &predicate, true).await?;
    let unaccent = tx.unaccent_status().await?;
    let mut compiled = compile_domain(tx, records, &resolved, unaccent).await?;
    if searched {
        // a start set found by a search: what the predicate lets through depends on data it
        // does not list, so a decision made with it is never remembered. A fresh `Arc` (never
        // shared), so this copies nothing.
        Arc::make_mut(&mut compiled).volatile = true;
    }
    Ok(compiled)
}

/// [`compile_predicate`] with placeholders numbered after `offset`: one column of a statement
/// that binds `offset` parameters before it (the access evidence statement).
pub(crate) async fn compile_predicate_at(
    tx: &mut Transaction,
    records: &Records,
    predicate: &Domain,
    offset: usize,
) -> Result<Arc<Compiled>> {
    let _search_scope = crate::search_method::scope(tx)?;
    let records = &predicate_records(records);
    let security = records.clone();
    let predicate =
        crate::search_method::expand_policy(tx, security.env(), security.spec(), predicate)?;
    tx.prepare_access_for_domain(security.env(), &predicate)
        .await?;
    let (resolved, searched) =
        crate::hierarchy::resolve_tracking(tx, &security.engine_sudo(), &predicate, true).await?;
    let unaccent = tx.unaccent_status().await?;
    let mut compiled = compile_domain_at(tx, records, &resolved, unaccent, offset).await?;
    if searched {
        Arc::make_mut(&mut compiled).volatile = true;
    }
    Ok(compiled)
}

/// The compiled order `order` (the model's `_order` when empty) of `records`' model.
#[derive(Debug, Clone)]
pub(crate) struct BoundOrder {
    order: Arc<OrderBy>,
    pub(crate) params: Vec<Param>,
}
impl std::ops::Deref for BoundOrder {
    type Target = OrderBy;
    fn deref(&self) -> &OrderBy {
        &self.order
    }
}

pub(crate) async fn order_for(
    tx: &mut Transaction,
    records: &Records,
    order: &str,
    offset: usize,
) -> Result<Arc<BoundOrder>> {
    compile_search_order(tx, records, order, offset).await
}

async fn compile_search_order(
    tx: &mut Transaction,
    records: &Records,
    order: &str,
    offset: usize,
) -> Result<Arc<BoundOrder>> {
    tx.validate_cached_search_language(records.env())?;
    let spec = records.spec();
    let registry = records.env().registry();
    let lang = records.env().lang();
    let fields = erp_sql::order::order_fields(registry, spec, order, lang)?;
    let company = tx.company_query_values(records.env(), &fields).await?;
    let (compiled, params) = match company {
        Some(values) => (
            Arc::new(erp_sql::order::compile_order_with_company(
                registry,
                spec,
                order,
                lang,
                &values,
                offset + 1,
            )?),
            vec![values.param()?],
        ),
        None => (
            tx.shared
                .search_memo
                .order(records.model_id(), order, lang, || {
                    compile_order(registry, spec, order, lang)
                })?,
            Vec::new(),
        ),
    };
    tx.check_read_pairs(records.env(), &compiled.fields)?;
    Ok(Arc::new(BoundOrder {
        order: compiled,
        params,
    }))
}

/// Collect into `out` the stored column fields field `fid` reads, following the dependencies of
/// non-stored computed fields (declared `depends`, `related` first hops, fields a compute reads)
/// on the same model. A field with a column is itself the answer; `display_name` yields the rec
/// name column it is computed from; `id` yields nothing. `visited` stops cycles.
pub(crate) fn resolve_stored_dependencies(
    spec: &ModelSpec,
    fid: FieldId,
    graph: Option<&ModelGraph>,
    visited: &mut FxHashSet<FieldId>,
    out: &mut Vec<FieldId>,
) {
    if !visited.insert(fid) {
        return;
    }
    let field = spec.field_by_id(fid);
    if field.has_column() {
        if !out.contains(&fid) {
            out.push(fid);
        }
        return;
    }
    if graph.is_some_and(|graph| graph.is_builtin_display_name(fid))
        // Graph-free planning has only declaration metadata. Preserve its fallback;
        // runtime planning below supplies the resolved graph for custom dependencies.
        || (graph.is_none()
            && field.name == "display_name"
            && field.compute == Some("_compute_display_name"))
    {
        if let Some(rec) = spec.rec_name.filter(|f| spec.field_by_id(*f).has_column())
            && !out.contains(&rec)
        {
            out.push(rec);
        }
        return;
    }
    if matches!(field.ty, FieldType::Id) {
        return;
    }
    if let Some(fc) = graph.and_then(|g| g.compute(fid)) {
        for &dep_id in &fc.depends {
            resolve_stored_dependencies(spec, dep_id, graph, visited, out);
        }
        for &read_id in &fc.readable {
            resolve_stored_dependencies(spec, read_id, graph, visited, out);
        }
    } else {
        if let Some(path) = field.related {
            let first = path.split('.').next().unwrap_or("");
            if let Some(dep) = spec.field(first) {
                resolve_stored_dependencies(spec, dep.id, graph, visited, out);
            }
        }
        if let Some(depends) = field.depends {
            for dotname in depends {
                let first = dotname.split('.').next().unwrap_or("");
                if let Some(dep) = spec.field(first) {
                    resolve_stored_dependencies(spec, dep.id, graph, visited, out);
                }
            }
        }
        for dotname in field.reads {
            let first = dotname.split('.').next().unwrap_or("");
            if let Some(dep) = spec.field(first) {
                resolve_stored_dependencies(spec, dep.id, graph, visited, out);
            }
        }
    }
}

fn fields_to_fetch(
    spec: &ModelSpec,
    names: Option<&[&str]>,
    graph: Option<&ModelGraph>,
) -> Result<Vec<FieldId>> {
    let Some(names) = names else {
        return Ok(spec.prefetch_fields.clone());
    };
    let mut out = Vec::with_capacity(names.len());
    let mut visited = FxHashSet::default();
    for name in names {
        let field = spec.field(name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{name}' on '{}'", spec.name))
        })?;
        resolve_stored_dependencies(spec, field.id, graph, &mut visited, &mut out);
    }
    Ok(out)
}

pub(crate) fn checked_columns(
    tx: &Transaction,
    env: &Env,
    spec: &ModelSpec,
    fetch: &Fetch,
) -> Result<Vec<FieldId>> {
    let mut columns = match fetch {
        Fetch::Fields(names) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            fields_to_fetch(spec, Some(&names), Some(tx.shared.graph.model(spec.id)))?
        }
        Fetch::Ids | Fetch::Prefetchable => fetch_columns(spec, fetch)?,
    };
    match fetch {
        Fetch::Ids => {}
        Fetch::Fields(names) => {
            tx.check_field_names(
                env,
                spec.id,
                names.iter().map(String::as_str),
                FieldOp::Read,
            )?;
        }
        Fetch::Prefetchable => {
            let mask = tx.field_mask(env, spec.id)?;
            if !mask.is_all() {
                columns.retain(|f| mask.readable(*f));
            }
        }
    }
    Ok(columns)
}

pub(crate) fn fetch_columns(spec: &ModelSpec, fetch: &Fetch) -> Result<Vec<FieldId>> {
    match fetch {
        Fetch::Ids => Ok(Vec::new()),
        Fetch::Prefetchable => fields_to_fetch(spec, None, None),
        Fetch::Fields(names) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            fields_to_fetch(spec, Some(&names), None)
        }
    }
}

fn count_param(n: u64) -> Result<Param> {
    i64::try_from(n)
        .map(Param::Count)
        .map_err(|_| Error::programming(format!("limit or offset out of range: {n}")))
}

/// How a statement is planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanPolicy {
    /// A named prepared statement cached on the connection: PostgreSQL switches to a generic plan
    /// after five executions. For statements whose parameters are ids, row counts and DML arrays.
    Cached,
    /// An unnamed statement planned for the values of each call (a custom plan), in one round trip
    /// and not kept on the connection. For statements filtered by domain literals.
    PerCall,
}

impl PlanPolicy {
    /// `PerCall` when the statement has parameters other than `LIMIT`/`OFFSET` counts.
    pub(crate) fn of(params: &[Param]) -> Self {
        if params.iter().any(|p| !matches!(p, Param::Count(_))) {
            Self::PerCall
        } else {
            Self::Cached
        }
    }
}

/// The driver type of a parameter, matching the cast of its placeholder.
pub(crate) fn param_type(param: &Param) -> Type {
    let scalar = |kind: ScalarKind, array: bool| match (kind, array) {
        (ScalarKind::Int8, false) => Type::INT8,
        (ScalarKind::Float8, false) => Type::FLOAT8,
        (ScalarKind::Numeric, false) => Type::NUMERIC,
        (ScalarKind::Text, false) => Type::TEXT,
        (ScalarKind::Date, false) => Type::DATE,
        (ScalarKind::Timestamp, false) => Type::TIMESTAMP,
        (ScalarKind::Uuid, false) => Type::UUID,
        (ScalarKind::Uuid, true) => Type::UUID_ARRAY,
        (ScalarKind::Xid8, false) => Type::XID8,
        (ScalarKind::Xid8, true) => Type::XID8_ARRAY,
        (ScalarKind::TimestampTz, false) => Type::TIMESTAMPTZ,
        (ScalarKind::TimestampTz, true) => Type::TIMESTAMPTZ_ARRAY,

        (ScalarKind::Int8, true) => Type::INT8_ARRAY,
        (ScalarKind::Float8, true) => Type::FLOAT8_ARRAY,
        (ScalarKind::Numeric, true) => Type::NUMERIC_ARRAY,
        (ScalarKind::Text, true) => Type::TEXT_ARRAY,
        (ScalarKind::Date, true) => Type::DATE_ARRAY,
        (ScalarKind::Timestamp, true) => Type::TIMESTAMP_ARRAY,
        (ScalarKind::DateRange, false) => Type::DATE_RANGE,
        (ScalarKind::TsRange, false) => Type::TS_RANGE,
        (ScalarKind::DateRange, true) => Type::DATE_RANGE_ARRAY,
        (ScalarKind::TsRange, true) => Type::TS_RANGE_ARRAY,
    };
    match param {
        Param::Count(_) => Type::INT8,
        Param::Scalar(s) => scalar(s.kind(), false),
        Param::Array(kind, _) => scalar(*kind, true),
        Param::Principal { kind, array, .. } => scalar(*kind, *array),
    }
}

/// Bound parameter values.
pub(crate) type Bound = Vec<Box<dyn ToSql + Sync + Send>>;

/// [`bind`] with each value's driver type (for unnamed typed statements).
pub(crate) fn typed_params(params: &[Param]) -> Result<(Bound, Vec<Type>)> {
    Ok((bind(params)?, params.iter().map(param_type).collect()))
}

/// Parameters as driver values. Arrays are typed vectors (one `T[]` parameter per collection).
pub(crate) fn bind(params: &[Param]) -> Result<Vec<Box<dyn ToSql + Sync + Send>>> {
    fn collect<T>(items: &[Scalar], pick: impl Fn(&Scalar) -> Option<T>) -> Result<Vec<T>> {
        items
            .iter()
            .map(|s| {
                pick(s).ok_or_else(|| Error::programming("array parameter with mixed value types"))
            })
            .collect()
    }
    params
        .iter()
        .map(|param| -> Result<Box<dyn ToSql + Sync + Send>> {
            Ok(match param {
                Param::Principal { param, .. } => {
                    return Err(Error::programming(format!(
                        "the principal parameter {} reached a statement unbound",
                        param.symbol()
                    )));
                }
                Param::Count(n) => Box::new(*n),
                Param::Scalar(Scalar::Int8(v)) => Box::new(*v),
                Param::Scalar(Scalar::Float8(v)) => Box::new(*v),
                Param::Scalar(Scalar::Numeric(v)) => Box::new(*v),
                Param::Scalar(Scalar::Text(v)) => Box::new(v.to_string()),
                Param::Scalar(Scalar::Date(v)) => Box::new(*v),
                Param::Scalar(Scalar::Timestamp(v)) => Box::new(*v),
                Param::Scalar(Scalar::Uuid(v)) => Box::new(*v),
                Param::Scalar(Scalar::Xid8(v)) => Box::new(*v),
                Param::Scalar(Scalar::TimestampTz(v)) => Box::new(*v),

                Param::Scalar(Scalar::DateRange(r, b)) => {
                    Box::new(erp_fields::RangeSql::new(*r, *b))
                }
                Param::Scalar(Scalar::TsRange(r, b)) => Box::new(erp_fields::RangeSql::new(*r, *b)),
                Param::Array(kind, items) => match kind {
                    ScalarKind::Int8 => Box::new(collect(items, |s| match s {
                        Scalar::Int8(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Float8 => Box::new(collect(items, |s| match s {
                        Scalar::Float8(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Numeric => Box::new(collect(items, |s| match s {
                        Scalar::Numeric(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Text => Box::new(collect(items, |s| match s {
                        Scalar::Text(v) => Some(v.to_string()),
                        _ => None,
                    })?),
                    ScalarKind::Date => Box::new(collect(items, |s| match s {
                        Scalar::Date(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Timestamp => Box::new(collect(items, |s| match s {
                        Scalar::Timestamp(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Uuid => Box::new(collect(items, |s| match s {
                        Scalar::Uuid(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::Xid8 => Box::new(collect(items, |s| match s {
                        Scalar::Xid8(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::TimestampTz => Box::new(collect(items, |s| match s {
                        Scalar::TimestampTz(v) => Some(*v),
                        _ => None,
                    })?),
                    ScalarKind::DateRange => Box::new(collect(items, |s| match s {
                        Scalar::DateRange(r, b) => Some(erp_fields::RangeSql::new(*r, *b)),
                        _ => None,
                    })?),
                    ScalarKind::TsRange => Box::new(collect(items, |s| match s {
                        Scalar::TsRange(r, b) => Some(erp_fields::RangeSql::new(*r, *b)),
                        _ => None,
                    })?),
                },
            })
        })
        .collect()
}

pub(crate) fn decode_id(row: &(impl CellSource + ?Sized)) -> Result<RecId> {
    let raw: i64 = row
        .cell(0)
        .map_err(|e| Error::new(ErrorKind::Database, "could not decode an id").with_source(e))?;
    RecId::from_real(raw)
        .ok_or_else(|| Error::programming(format!("the database returned the id {raw}")))
}

/// Flush what a query reads: the listed fields, or the whole model when an audit
/// field is read (every pending write of a record changes its `write_date`/`write_uid` at flush).
pub(crate) async fn flush_read_inputs(
    tx: &mut Transaction,
    fields: &[(ModelId, FieldId)],
) -> Result<()> {
    let registry = Arc::clone(tx.registry());
    let mut models: Vec<ModelId> = fields.iter().map(|(m, _)| *m).collect();
    models.sort_unstable();
    models.dedup();
    for model in models {
        let spec = registry.model_by_id(model);
        let wanted: Vec<FieldId> = fields
            .iter()
            .filter(|(m, _)| *m == model)
            .map(|(_, f)| *f)
            .collect();
        // boxed: a flush may recompute, and a compute may search again
        if wanted.iter().any(|f| spec.field_by_id(*f).magic) {
            Box::pin(tx.flush_model(model)).await?;
        } else {
            Box::pin(tx.flush_fields(model, &wanted)).await?;
        }
    }
    Ok(())
}

impl Transaction {
    /// The memoized text of a predicate shape with placeholders after `offset`. Decisions of a
    /// private policy snapshot never populate a cache that outlives the transaction (design note
    /// §2.3, Revision 3 M1): their texts go to a memo of the transaction.
    pub(crate) fn search_memo_text(
        &self,
        key: &ShapeKey,
        offset: usize,
        render: impl FnOnce() -> erp_sql::filter::Rendered,
    ) -> MemoText {
        let key = (key.clone(), offset);
        if self.policy().snapshot().is_shared() {
            return self.shared.search_memo.text(&key, render);
        }
        let found = self
            .private_texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(text) = found {
            return text;
        }
        let rendered = render();
        let text = MemoText {
            sql: rendered.sql.into(),
            joins: rendered.joins.into(),
        };
        self.private_texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, text.clone());
        text
    }

    pub(crate) async fn run_params(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>> {
        let policy = self.plan_policy(params);
        self.run_params_with(sql, params, policy).await
    }

    /// [`Transaction::run_params`] with an explicit plan policy (statements that bind ids next to
    /// optional domain literals decide from the literals alone).
    pub(crate) async fn run_params_with(
        &mut self,
        sql: &str,
        params: &[Param],
        policy: PlanPolicy,
    ) -> Result<Vec<Row>> {
        let bound = bind(params)?;
        let refs: Vec<&(dyn ToSql + Sync)> = bound
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();
        match policy {
            PlanPolicy::Cached => self.query(sql, &refs).await,
            PlanPolicy::PerCall => {
                let typed: Vec<(&(dyn ToSql + Sync), Type)> = refs
                    .iter()
                    .zip(params)
                    .map(|(value, param)| (*value, param_type(param)))
                    .collect();
                self.query_per_call(sql, &typed).await
            }
        }
    }
}

/// Decodes the rows of a fetch (`id`, then `fields` in order) into the cache without overwriting
/// pending values (design note `perf-stream.md` §3.1). Built once per statement or flight, used
/// for every row: the columns to decode are resolved once.
pub(crate) struct RowCacher {
    model: ModelId,
    /// `(field, column index in the row, column type, storage)` of each field with a column.
    columns: Vec<(FieldId, usize, ColumnType, Storage)>,
}

impl RowCacher {
    /// The decoder of rows `id, fields…` of `spec`; fields without a column are skipped, as they
    /// are not selected.
    pub(crate) fn new(spec: &ModelSpec, fields: &[FieldId]) -> Self {
        let columns = fields
            .iter()
            .enumerate()
            .filter_map(|(i, fid)| {
                let field = spec.field_by_id(*fid);
                // a field without a column is not selected (not a skipped value)
                let column = field.column()?;
                Some((*fid, i + 1, column, field.storage()))
            })
            .collect();
        Self {
            model: spec.id,
            columns,
        }
    }

    /// The per-row sink of this decoder for one fetch of `expected` rows (0 when the count is not
    /// known). Nothing happens to the cache before the first row: an empty answer reserves
    /// nothing and copies no model into an open snapshot layer (perf-stream review D3).
    pub(crate) fn sink(&self, expected: usize) -> RowSink<'_> {
        RowSink {
            cacher: self,
            expected,
            seen: 0,
            writer: None,
        }
    }
}

/// The per-row half of a [`RowCacher`], from [`RowCacher::sink`]; one per fetch, which may span
/// several statements (the chunks of a read).
pub(crate) struct RowSink<'a> {
    cacher: &'a RowCacher,
    /// Rows the fetch returns at most, when known (0 otherwise).
    expected: usize,
    /// Rows handed over so far.
    seen: usize,
    /// Taken on the first row that has a column to fill: the model saved once in the open
    /// snapshot layers, not on every row (review F5).
    writer: Option<ModelWriter>,
}

impl RowSink<'_> {
    /// Cache one row; its id. A cell whose value is already cached is not decoded (W9). Room for
    /// the rest of the fetch is made when the row needs the cache to grow
    /// ([`crate::cache::ModelCache::fetched_slot`]).
    ///
    /// # Errors
    ///
    /// An id or a value that cannot be decoded, a value of the wrong kind for its column.
    pub(crate) fn cache_row(&mut self, cache: &mut Cache, row: &RawRow<'_>) -> Result<RecId> {
        let id = decode_id(row)?;
        if self.cacher.columns.is_empty() {
            return Ok(id);
        }
        let remaining = crate::cache::rows_to_come(self.expected, self.seen);
        self.seen = self.seen.saturating_add(1);
        let model = self.cacher.model;
        let writer = *self.writer.get_or_insert_with(|| cache.writer(model));
        let model = cache.write(writer);
        let at = model.fetched_slot(id, remaining);
        for (field, index, column, storage) in &self.cacher.columns {
            model.fill_fetched(at, *field, *storage, || {
                crate::read::decode(row, *index, *column)
            })?;
        }
        Ok(id)
    }
}

// ----- reads of explicit records -----------------------------------------------------------------

/// The access predicate restricting reads of `records`' model in its environment:
/// `None` in superuser mode or when the policy's access domain is `TRUE`. The model check has
/// already run.
pub(crate) async fn read_restriction(
    tx: &mut Transaction,
    records: &Records,
) -> Result<Option<Arc<Compiled>>> {
    record_restriction(tx, records, Operation::Read).await
}

/// The access predicate of `op` on `records`' model in its environment, as
/// [`read_restriction`] computes it for reads: `None` in superuser mode or when the policy's
/// access domain is `TRUE`. The upsert's write pre-check and create check use it for `Write` and
/// `Create` (design note §3.5).
pub(crate) async fn record_restriction(
    tx: &mut Transaction,
    records: &Records,
    op: Operation,
) -> Result<Option<Arc<Compiled>>> {
    record_restriction_at(tx, records, op, 0).await
}

/// [`record_restriction`] with placeholders numbered after `offset`, for a predicate that joins
/// another one in the same statement (the upsert's write pre-check reads the read rule next to
/// the write rule).
pub(crate) async fn record_restriction_at(
    tx: &mut Transaction,
    records: &Records,
    op: Operation,
    offset: usize,
) -> Result<Option<Arc<Compiled>>> {
    let env = records.env();
    env.companies()?;
    if env.su() {
        return Ok(None);
    }
    tx.prepare_access(env).await?;
    let access = tx.access_predicate(env, records.model_id(), op)?;
    if access.is_true() {
        return Ok(None);
    }
    if access.is_false() {
        return Ok(Some(false_compiled()));
    }
    compile_predicate_at(tx, records, &access, offset)
        .await
        .map(Some)
}

/// The statements of a fetch: one text, `SELECT id, fields… WHERE id = ANY($n)
/// [AND <restriction>]`, sent once per chunk of at most `chunk` ids. A chunk's id parameter is
/// built when its statement is sent, and every chunk shares the text and the restriction's
/// literals (perf-audit R17).
pub(crate) struct FetchStatements {
    sql: Arc<str>,
    /// The restriction's literals; the id array is the last parameter.
    literals: Vec<Param>,
    policy: PlanPolicy,
    chunk: usize,
}

impl FetchStatements {
    /// The statements fetching `fields` of `spec` under `restriction`; `None` when the
    /// restriction lets nothing through (no statement at all).
    fn new(
        registry: &erp_registry::Registry,
        spec: &ModelSpec,
        fields: &[FieldId],
        restriction: Option<&Compiled>,
        chunk: usize,
    ) -> Option<Self> {
        if restriction.is_some_and(|f| f.constant == Some(false)) {
            return None;
        }
        let restriction = restriction.filter(|f| f.constant != Some(true));
        let joins = restriction
            .map(|f| merge_joins(&[&f.join_list]))
            .unwrap_or_default();
        let literals = restriction.map(|f| f.params.clone()).unwrap_or_default();
        let sql = select_ids_where_cached(
            registry,
            spec,
            fields,
            &joins,
            restriction.map(|f| &*f.sql),
            literals.len() + 1,
        );
        // ids plus the restriction's literals: planned per call only when there are literals
        let policy = if literals.is_empty() {
            PlanPolicy::Cached
        } else {
            PlanPolicy::PerCall
        };
        Some(Self {
            sql,
            literals,
            policy,
            chunk: chunk.max(1),
        })
    }

    /// The number of statements for `ids` ids.
    fn count(&self, ids: usize) -> usize {
        ids.div_ceil(self.chunk)
    }

    /// The statements of `ids` for a flight, each with its parameters bound.
    fn flight<'s>(&'s self, ids: &[RealId]) -> Result<Vec<PipelinedQuery<'s>>> {
        let types: Vec<Type> = self
            .literals
            .iter()
            .map(param_type)
            .chain([Type::INT8_ARRAY])
            .collect();
        let mut out = Vec::with_capacity(self.count(ids.len()));
        for chunk in ids.chunks(self.chunk) {
            let mut values = bind(&self.literals)?;
            values.push(Box::new(id_values(chunk)));
            out.push(PipelinedQuery::bound(
                &self.sql,
                values,
                types.clone(),
                self.policy,
            ));
        }
        Ok(out)
    }

    /// Run the statements of `ids` one after the other, caching each row as it arrives; the ids
    /// found. One id buffer serves every chunk.
    async fn run(
        &self,
        tx: &mut Transaction,
        ids: &[RealId],
        cacher: &RowCacher,
    ) -> Result<FxHashSet<RecId>> {
        let mut fetched = FxHashSet::with_capacity_and_hasher(ids.len(), Default::default());
        let literals = bind(&self.literals)?;
        let literal_types: Vec<Type> = self.literals.iter().map(param_type).collect();
        let mut chunk_ids: Vec<i64> = Vec::with_capacity(ids.len().min(self.chunk));
        // the row count of every chunk together is known: the cache grows once
        let mut rows = cacher.sink(ids.len());
        for chunk in ids.chunks(self.chunk) {
            chunk_ids.clear();
            chunk_ids.extend(chunk.iter().map(|id| id.get()));
            let mut refs: Vec<&(dyn ToSql + Sync)> = literals
                .iter()
                .map(|p| p.as_ref() as &(dyn ToSql + Sync))
                .collect();
            refs.push(&chunk_ids);
            let sink = |cache: &mut Cache, row: &RawRow<'_>| -> Result<()> {
                fetched.insert(rows.cache_row(cache, row)?);
                Ok(())
            };
            match self.policy {
                PlanPolicy::Cached => {
                    tx.query_each_args(&self.sql, Args::Prepared(&refs), sink)
                        .await?;
                }
                PlanPolicy::PerCall => {
                    let typed: Vec<(&(dyn ToSql + Sync), Type)> = refs
                        .iter()
                        .copied()
                        .zip(literal_types.iter().cloned().chain([Type::INT8_ARRAY]))
                        .collect();
                    tx.query_each_args(&self.sql, Args::Typed(&typed), sink)
                        .await?;
                }
            }
        }
        Ok(fetched)
    }
}

/// The `int8[]` parameter of a chunk of ids.
fn id_values(ids: &[RealId]) -> Vec<i64> {
    ids.iter().map(|id| id.get()).collect()
}

/// What [`Records::fetch_plan`] decided: the statements to send and what to do with the answers
/// ([`Records::fetch_finish`]).
pub(crate) struct FetchPlan {
    env: Env,
    model: ModelId,
    ids: Vec<RealId>,
    computed: Vec<FieldId>,
    probe: Option<FieldId>,
    work: FetchWork,
}

enum FetchWork {
    /// Everything is cached and nothing restricts the read.
    Nothing,
    /// Every value is cached; only the rules decide (one query at most, for the records not
    /// decided yet).
    Decide,
    /// Statements to send (none when the restriction lets nothing through); their answers are
    /// cached.
    Rows {
        missing: Vec<FieldId>,
        restriction: Option<Arc<Compiled>>,
        statements: Option<FetchStatements>,
    },
}

/// The ids a plan's statements found, already cached statement by statement as the answers came
/// in.
pub(crate) type Fetched = FxHashSet<RecId>;

impl FetchPlan {
    /// The number of statements the plan sends.
    pub(crate) fn statement_count(&self) -> usize {
        match &self.work {
            FetchWork::Rows {
                statements: Some(statements),
                ..
            } => statements.count(self.ids.len()),
            FetchWork::Rows {
                statements: None, ..
            }
            | FetchWork::Nothing
            | FetchWork::Decide => 0,
        }
    }

    /// The decoder of the plan's rows and its statements, when it sends any.
    fn rows(&self) -> Option<(RowCacher, &FetchStatements)> {
        match &self.work {
            FetchWork::Rows {
                missing,
                statements: Some(statements),
                ..
            } => {
                let registry = self.env.registry();
                let spec = registry.model_by_id(self.model);
                Some((RowCacher::new(spec, missing), statements))
            }
            FetchWork::Rows {
                statements: None, ..
            }
            | FetchWork::Nothing
            | FetchWork::Decide => None,
        }
    }

    /// Run the statements one after the other, caching each row as it arrives.
    async fn run_cached(&self, tx: &mut Transaction) -> Result<Fetched> {
        match self.rows() {
            Some((cacher, statements)) => statements.run(tx, &self.ids, &cacher).await,
            None => Ok(FxHashSet::default()),
        }
    }
}

/// `SELECT id, fields… WHERE id = ANY($n) [AND <restriction>]` per bounded chunk of `ids`, values
/// cached without overwriting pending writes; the ids found. With no fields it only tells which
/// records the restriction lets through. The caller flushes what the restriction reads. Each
/// row is cached as it arrives, so no statement's rows are held.
pub(crate) async fn fetch_rows(
    tx: &mut Transaction,
    spec: &ModelSpec,
    fields: &[FieldId],
    ids: &[RealId],
    restriction: Option<&Compiled>,
) -> Result<FxHashSet<RecId>> {
    match FetchStatements::new(
        tx.registry(),
        spec,
        fields,
        restriction,
        tx.config().prefetch_chunk,
    ) {
        Some(statements) => statements.run(tx, ids, &RowCacher::new(spec, fields)).await,
        None => Ok(FxHashSet::default()),
    }
}

/// Fetch the fields of several independent recordsets with their statements in one flight
/// (design note §4): the checks and access decisions of each run first, in order; the
/// statements of all of them are then sent together on the transaction's connection and their
/// rows cached as they arrive, statement by statement. The statements are the ones
/// [`Records::fetch`] would have sent, so the statement count does not change, only the round
/// trips. Recordsets of the same model must not appear twice: the second plan would not see what
/// the first caches.
///
/// A flight carries at most [`MAX_FLIGHT_STATEMENTS`](crate::transaction::MAX_FLIGHT_STATEMENTS)
/// statements: plans are grouped in order up to the cap (one more round trip per group), and a
/// plan with more statements than the cap by itself runs its statements one by one. The answers
/// of a group are cached, and the access decisions settled, before the non-stored computed
/// fields of any plan of the group are computed, so a compute reading a comodel of the group
/// finds it cached.
pub(crate) async fn fetch_together(
    tx: &mut Transaction,
    requests: &[(&Records, &[&str])],
) -> Result<()> {
    use crate::transaction::MAX_FLIGHT_STATEMENTS;
    let mut plans = Vec::with_capacity(requests.len());
    let mut models = FxHashSet::default();
    for (records, names) in requests {
        if !models.insert(records.model_id()) {
            return Err(Error::programming(format!(
                "fetch_together got two recordsets of model '{}'",
                records.spec().name
            )));
        }
        if let Some(plan) = records.fetch_plan(tx, Some(names)).await? {
            plans.push((*records, plan));
        }
    }
    let mut group: Vec<(&Records, FetchPlan)> = Vec::new();
    let mut count = 0;
    for (records, plan) in plans {
        let size = plan.statement_count();
        if size > MAX_FLIGHT_STATEMENTS {
            // too big for a flight of its own: the statements run one by one
            fetch_group(tx, std::mem::take(&mut group)).await?;
            count = 0;
            let fetched = plan.run_cached(tx).await?;
            records.fetch_finish(tx, plan, fetched).await?;
            continue;
        }
        if count + size > MAX_FLIGHT_STATEMENTS {
            fetch_group(tx, std::mem::take(&mut group)).await?;
            count = 0;
        }
        count += size;
        group.push((records, plan));
    }
    fetch_group(tx, group).await
}

/// One flight for the statements of `group`, each row cached as it arrives (statement by
/// statement, in order), then the access decisions settled and the computed fields computed.
async fn fetch_group(tx: &mut Transaction, group: Vec<(&Records, FetchPlan)>) -> Result<()> {
    if group.is_empty() {
        return Ok(());
    }
    let mut fetched: Vec<Fetched> = Vec::with_capacity(group.len());
    {
        let rows: Vec<Option<(RowCacher, &FetchStatements)>> =
            group.iter().map(|(_, plan)| plan.rows()).collect();
        let mut queries = Vec::new();
        // the plan (index into `group`) of each statement of the flight
        let mut owner: Vec<usize> = Vec::new();
        // the row sink of each plan that fetches rows
        let mut sinks: Vec<Option<RowSink<'_>>> = Vec::with_capacity(group.len());
        for (k, ((_, plan), rows)) in group.iter().zip(&rows).enumerate() {
            fetched.push(FxHashSet::with_capacity_and_hasher(
                if rows.is_some() { plan.ids.len() } else { 0 },
                Default::default(),
            ));
            match rows {
                Some((cacher, statements)) => {
                    // the row count of the whole plan is known: the cache grows once
                    sinks.push(Some(cacher.sink(plan.ids.len())));
                    let flight = statements.flight(&plan.ids)?;
                    owner.extend(std::iter::repeat_n(k, flight.len()));
                    queries.extend(flight);
                }
                None => sinks.push(None),
            }
        }
        tx.query_flight(&queries, |i, cache, row| {
            let missing = || Error::programming(format!("a flight row named statement {i}"));
            let k = *owner.get(i).ok_or_else(missing)?;
            let sink = sinks
                .get_mut(k)
                .and_then(Option::as_mut)
                .ok_or_else(missing)?;
            let id = sink.cache_row(cache, row)?;
            fetched.get_mut(k).ok_or_else(missing)?.insert(id);
            Ok(())
        })
        .await?;
    }
    let mut to_compute = Vec::with_capacity(group.len());
    for ((records, plan), found) in group.into_iter().zip(fetched) {
        let (surviving, computed) = records.fetch_apply(tx, plan, found).await?;
        to_compute.push((surviving, computed));
    }
    for (surviving, computed) in to_compute {
        surviving.compute_fetched(tx, computed).await?;
    }
    Ok(())
}

// ----- the lazy query ----------------------------------------------------------------------------

/// Running
/// it memoizes the ids; counting runs `COUNT(*)` unless the ids are known.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    records: Records,
    filter: Arc<Compiled>,
    order: Option<Arc<BoundOrder>>,
    limit: Option<u64>,
    offset: u64,
    ids: Option<Ids>,
    /// How the statement is planned.
    plan: PlanClass,
}

/// How a [`SearchQuery`]'s statement is planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanClass {
    /// By its parameters ([`Transaction::plan_policy`]): a caller's literals are planned per call.
    ByParams,
    /// An ORM-produced id-list fetch: exactly one parameter, the id list, read behind a sub-select
    /// (D2.5); a cached prepared statement ([`Transaction::plan_policy_id_list`]). Adding a
    /// literal or a limit returns the query to [`PlanClass::ByParams`]; the sub-select stays in
    /// the text, which plans the same per call.
    IdList,
}

impl SearchQuery {
    /// Whether the result is known to be empty without running a query (a `FALSE` domain, or an
    /// executed query that found nothing).
    pub fn is_empty(&self) -> bool {
        self.filter.constant == Some(false) || self.ids.as_ref().is_some_and(|ids| ids.is_empty())
    }

    /// The ids, once the query ran.
    pub fn known_ids(&self) -> Option<&Ids> {
        self.ids.as_ref()
    }

    /// The `WHERE` predicate (`TRUE` when unrestricted).
    pub fn predicate(&self) -> &str {
        &self.filter.sql
    }

    /// The joins of the statement: the predicate's, plus the order's when ordering.
    fn joins(&self, with_order: bool) -> String {
        match (&self.order, with_order) {
            (Some(order), true) => merge_joins(&[&self.filter.join_list, &order.joins]),
            _ => merge_joins(&[&self.filter.join_list]),
        }
    }

    fn clauses<'a>(
        &'a self,
        params: &mut Vec<Param>,
        with_order: bool,
        limit: Option<u64>,
        joins: &'a str,
    ) -> Result<SearchSql<'a>> {
        if with_order && let Some(order) = &self.order {
            params.extend(order.params.iter().cloned());
        }
        let limit = match limit {
            Some(n) => {
                params.push(count_param(n)?);
                Some(params.len())
            }
            None => None,
        };
        let offset = if self.offset > 0 {
            params.push(count_param(self.offset)?);
            Some(params.len())
        } else {
            None
        };
        Ok(SearchSql {
            joins: Some(joins),
            predicate: (self.filter.constant != Some(true)).then_some(&*self.filter.sql),
            order: if with_order {
                self.order.as_deref().map(|o| o.sql.as_str())
            } else {
                None
            },
            limit,
            offset,
        })
    }

    async fn flush_inputs(&self, tx: &mut Transaction) -> Result<()> {
        let mut fields = self.filter.fields.clone();
        if let Some(order) = &self.order {
            fields.extend(
                order
                    .fields
                    .iter()
                    .filter(|f| !self.filter.fields.contains(f)),
            );
        }
        flush_read_inputs(tx, &fields).await
    }

    /// Number of parameters of the filter (a keyset piece numbers its own after them).
    pub(crate) fn filter_params(&self) -> usize {
        self.filter.params.len() + self.order.as_ref().map_or(0, |order| order.params.len())
    }

    /// AND a keyset piece (`erp_sql::keyset`) to the filter; its placeholders are
    /// numbered after the filter's ([`SearchQuery::filter_params`]).
    pub(crate) fn restrict(&mut self, sql: &str, params: Vec<Param>) {
        // a FALSE filter matches nothing whatever is ANDed to it, and runs no query: keep it
        // constant instead of rendering `FALSE AND (…)`
        if self.filter.constant == Some(false) {
            return;
        }
        let mut filter = (*self.filter).clone();
        if let Some(order) = &mut self.order {
            filter.params.append(&mut Arc::make_mut(order).params);
        }
        filter.sql = if filter.constant == Some(true) {
            Arc::from(sql)
        } else {
            Arc::from(format!("({}) AND ({sql})", filter.sql))
        };
        filter.params.extend(params);
        filter.constant = None;
        self.filter = Arc::new(filter);
        self.ids = None;
        // a literal was added: no longer an id-list fetch
        self.plan = PlanClass::ByParams;
    }

    /// Plan the query as an ORM-produced id-list fetch: a cached prepared statement
    /// whose id list the planner reads behind a sub-select (D2.5,
    /// [`erp_sql::select::blind_id_list`]), so every plan of it is the generic one. Only for a
    /// query whose one literal is an id list the ORM built from records it holds (a caller's
    /// domain stays planned per call).
    ///
    /// # Errors
    ///
    /// `Programming` when the filter carries any other parameter, or none, or does not bind the
    /// id list as `= ANY($1::int8[])` exactly once.
    pub(crate) fn plan_as_id_list(&mut self) -> Result<()> {
        let blind = (self.filter.params.len() == 1 && self.limit.is_none() && self.offset == 0)
            .then(|| erp_sql::select::blind_id_list(&self.filter.sql))
            .flatten();
        let Some(sql) = blind else {
            return Err(Error::programming(format!(
                "an id-list fetch of {} must have the id list `= ANY($1::int8[])` as its only \
                 parameter; it has {} parameters (limit {:?}, offset {}) in {}",
                self.records.model_name(),
                self.filter.params.len(),
                self.limit,
                self.offset,
                self.filter.sql
            )));
        };
        let mut filter = (*self.filter).clone();
        filter.sql = Arc::from(sql);
        self.filter = Arc::new(filter);
        self.plan = PlanClass::IdList;
        Ok(())
    }

    /// Set the limit (a keyset piece asks for what the page still lacks).
    pub(crate) fn set_limit(&mut self, limit: u64) {
        self.limit = Some(limit);
        self.ids = None;
        self.plan = PlanClass::ByParams;
    }

    /// Run the query (unless it is known to be empty) fetching `columns` into the cache; the ids
    /// in query order. The model's order applies when none was set.
    pub(crate) async fn execute_columns(
        &mut self,
        tx: &mut Transaction,
        columns: &[FieldId],
    ) -> Result<Vec<RecId>> {
        tx.admit_records(&self.records).await?;
        tx.validate_search_language(self.records.env()).await?;
        if self.order.is_none() {
            self.order =
                Some(compile_search_order(tx, &self.records, "", self.filter.params.len()).await?);
        }
        if self.is_empty() {
            return Ok(Vec::new());
        }
        self.execute(tx, columns).await
    }

    /// The `SELECT` of the query fetching `columns`, with its parameters.
    pub(crate) fn statement(&self, columns: &[FieldId]) -> Result<(Arc<str>, Vec<Param>)> {
        let spec = self.records.spec();
        let mut params = self.filter.params.clone();
        let joins = self.joins(true);
        let clauses = self.clauses(&mut params, true, self.limit, &joins)?;
        let sql = select_cached(self.records.env().registry(), spec, columns, &clauses);
        Ok((sql, params))
    }

    /// Run the query fetching `columns` into the cache; the ids in query order.
    pub(crate) async fn execute(
        &self,
        tx: &mut Transaction,
        columns: &[FieldId],
    ) -> Result<Vec<RecId>> {
        tx.admit_records(&self.records).await?;
        if self.filter.constant == Some(false) {
            return Ok(Vec::new());
        }
        self.flush_inputs(tx).await?;
        let (sql, params) = self.statement(columns)?;
        let cacher = RowCacher::new(self.records.spec(), columns);
        // a limit bounds the rows: the ids and the cache grow once up to a chunk; an unbounded
        // search grows them in proportion to the rows it has added (`cache::rows_to_come`;
        // design note `perf-stream.md` §3.1, §11 risk 7)
        let chunk = tx.config().prefetch_chunk;
        // a limit too large for `usize` is larger than the chunk: the reservation is the chunk
        let expected = self.limit.map_or(0, |limit| {
            usize::try_from(limit).map_or(chunk, |limit| limit.min(chunk))
        });
        let mut ids = Vec::with_capacity(expected);
        let policy = match self.plan {
            PlanClass::IdList => tx.plan_policy_id_list(),
            PlanClass::ByParams => tx.plan_policy(&params),
        };
        let mut rows = cacher.sink(expected);
        tx.query_each_with(&sql, &params, policy, |cache, row| {
            ids.push(rows.cache_row(cache, row)?);
            Ok(())
        })
        .await?;
        Ok(ids)
    }

    /// The matching ids in order (one query the first time, none afterwards).
    pub async fn ids(&mut self, tx: &mut Transaction) -> Result<Ids> {
        tx.admit_records(&self.records).await?;
        if let Some(ids) = &self.ids {
            return Ok(Arc::clone(ids));
        }
        let ids: Ids = self.execute(tx, &[]).await?.into();
        self.ids = Some(Arc::clone(&ids));
        Ok(ids)
    }

    /// The matching records, sharing one prefetch set.
    pub async fn records(&mut self, tx: &mut Transaction) -> Result<Records> {
        let ids = self.ids(tx).await?;
        Ok(self.records.browse_ids(ids.iter().copied()))
    }

    pub async fn count(&self, tx: &mut Transaction) -> Result<u64> {
        tx.admit_records(&self.records).await?;
        if let Some(ids) = &self.ids {
            return Ok(ids.len() as u64);
        }
        if self.filter.constant == Some(false) {
            return Ok(0);
        }
        self.flush_inputs(tx).await?;
        let limit = self.limit.filter(|n| *n > 0 || self.offset > 0);
        let mut params = self.filter.params.clone();
        let joins = self.joins(false);
        let clauses = self.clauses(&mut params, false, limit, &joins)?;
        let sql = count(&self.records.spec().table, &clauses);
        let rows = tx.run_params(&sql, &params).await?;
        let n: i64 = rows
            .first()
            .ok_or_else(|| Error::programming("COUNT(*) returned no row"))?
            .try_get(0)
            .map_err(|e| {
                Error::new(ErrorKind::Database, "could not decode a count").with_source(e)
            })?;
        u64::try_from(n).map_err(|_| Error::programming(format!("COUNT(*) returned {n}")))
    }
}

// Reject opaque/custom operands before optimization can rewrite them into query-valued
// conditions. Charge every AST/value node before visiting it, including policy predicates.
fn validate_catalog_domain(
    domain: &Domain,
    spec: &ModelSpec,
    charge: &mut (dyn FnMut(usize) -> Result<()> + Send),
    depth: usize,
) -> Result<()> {
    charge(1)?;
    if depth >= erp_domain::MAX_DEPTH {
        return Err(Error::new(
            ErrorKind::ResourceExhausted,
            "selection catalog domain depth limit exceeded",
        ));
    }
    match domain.kind() {
        DomainKind::True | DomainKind::False => Ok(()),
        DomainKind::Not(child) => validate_catalog_domain(child, spec, charge, depth + 1),
        DomainKind::And(children) | DomainKind::Or(children) => {
            for child in children {
                validate_catalog_domain(child, spec, charge, depth + 1)?;
            }
            Ok(())
        }
        DomainKind::Custom(_) => Err(Error::programming(
            "selection catalogs require scalar domain conditions, not custom nodes",
        )),
        DomainKind::Condition(condition) => {
            let field = spec.field(condition.field_expr()).ok_or_else(|| {
                Error::programming("selection catalogs require direct stored scalar domain fields")
            })?;
            if !field.has_column()
                || matches!(field.ty, FieldType::Json)
                || matches!(
                    condition.operator(),
                    Operator::Any
                        | Operator::NotAny
                        | Operator::AnyBypass
                        | Operator::NotAnyBypass
                        | Operator::ChildOf
                        | Operator::ParentOf
                        | Operator::Access
                )
            {
                return Err(Error::programming(
                    "selection catalogs require direct stored scalar domain fields and operators",
                ));
            }
            validate_catalog_value(condition.value(), charge, depth + 1)
        }
    }
}

fn validate_catalog_value(
    value: &erp_domain::Value,
    charge: &mut (dyn FnMut(usize) -> Result<()> + Send),
    depth: usize,
) -> Result<()> {
    charge(1)?;
    if depth >= erp_domain::MAX_DEPTH {
        return Err(Error::new(
            ErrorKind::ResourceExhausted,
            "selection catalog domain depth limit exceeded",
        ));
    }
    match value {
        erp_domain::Value::Opaque(_) | erp_domain::Value::Domain(_) => Err(Error::programming(
            "selection catalogs require literal or principal-parameter domain values",
        )),
        erp_domain::Value::List(values) => {
            for value in values {
                validate_catalog_value(value, charge, depth + 1)?;
            }
            Ok(())
        }
        erp_domain::Value::Set(values) => {
            for value in values.iter() {
                validate_catalog_value(value, charge, depth + 1)?;
            }
            Ok(())
        }
        erp_domain::Value::Bool(_)
        | erp_domain::Value::Int(_)
        | erp_domain::Value::Float(_)
        | erp_domain::Value::Str(_)
        | erp_domain::Value::Date(_)
        | erp_domain::Value::Datetime(_)
        | erp_domain::Value::NewId
        | erp_domain::Value::Param(_) => Ok(()),
    }
}

fn refuse_catalog_properties(
    registry: &erp_registry::Registry,
    fields: &[(ModelId, FieldId)],
) -> Result<()> {
    for &(model, field) in fields {
        let model = registry.model_by_id(model);
        let field = model.field_by_id(field);
        if field.company_dependent {
            return Err(crate::company_dependent_runtime::unsupported(
                model.name,
                field.name,
                "selection catalog expressions",
            ));
        }
    }
    Ok(())
}

// ----- public API --------------------------------------------------------------------------------

impl Records {
    /// Internal bounded catalog path: no model methods, compute callbacks or implicit flushes.
    pub(crate) async fn selection_catalog(
        &self,
        tx: &mut Transaction,
        request: CatalogRequest<'_>,
    ) -> Result<Vec<Row>> {
        tx.validate_cached_search_language(self.env())?;
        let CatalogRequest {
            domain,
            fields: [key, label],
            order,
            pairs,
            bytes,
            charge,
        } = request;
        tx.admit_records(self).await?;
        let env = self.env();
        env.companies()?;
        tx.check_model(env, self.model_id(), Operation::Read)?;
        tx.check_field_names(env, self.model_id(), [key, label], FieldOp::Read)?;
        let spec = self.spec();
        let mut columns = Vec::new();
        for name in [key, label] {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!("Invalid catalog field '{name}' on '{}'", spec.name))
            })?;
            if !field.has_column() || !matches!(field.ty, FieldType::Char { .. } | FieldType::Text)
            {
                return Err(Error::programming(format!(
                    "selection catalog '{}.{name}' requires stored text",
                    spec.name
                )));
            }
            columns.push(field.id);
        }
        // Do not enter hierarchy/search callbacks while executing a native provider. Domains
        // outside the stored scalar subset refuse; no policy predicate is discarded or sudoed.
        let access = if env.su() {
            None
        } else {
            Some(tx.access_predicate(env, self.model_id(), Operation::Read)?)
        };
        for domain in std::iter::once(domain).chain(access.iter()) {
            validate_catalog_domain(domain, spec, charge, 0)?;
        }
        tx.check_domain_fields(env, spec, domain)?;
        // A native catalog must not trigger property-default lookups or an implicit flush.
        // Inspect the complete dependency plans, including user paths in record rules, first.
        let unaccent = tx.unaccent_status().await?;
        let registry = env.registry();
        let mut dependencies = erp_sql::order::order_fields(registry, spec, order, env.lang())?;
        dependencies.extend(columns.iter().map(|field| (spec.id, *field)));
        if let Some(active) = spec.active_name {
            dependencies.push((spec.id, active));
        }
        for (domain, policy) in
            std::iter::once((domain, false)).chain(access.iter().map(|d| (d, true)))
        {
            let records = if policy {
                predicate_records(self)
            } else {
                self.clone()
            };
            let prepared = prepare_sql_domain(tx, &records, domain)?;
            dependencies.extend(erp_sql::filter::where_fields(
                &prepared,
                &WhereContext {
                    registry,
                    model: spec,
                    unaccent,
                    lang: env.lang(),
                },
            )?);
        }
        refuse_catalog_properties(registry, &dependencies)?;
        let filter = compile_search(tx, self, domain, access.as_ref(), true, unaccent).await?;
        if filter.constant == Some(false) {
            return Ok(Vec::new());
        }
        let order = compile_search_order(tx, self, order, filter.params.len()).await?;
        for &(model, field) in &order.fields {
            if model != self.model_id()
                || !spec.field_by_id(field).has_column()
                || matches!(spec.field_by_id(field).ty, FieldType::Many2one { .. })
            {
                return Err(Error::programming(
                    "selection catalogs require direct stored scalar order fields",
                ));
            }
        }
        erp_sql::order::check_join_conflicts(&[&filter.join_list, &order.joins])?;
        let joins = merge_joins(&[&filter.join_list, &order.joins]);
        let mut params = filter.params.clone();
        params.extend(order.params.iter().cloned());
        let candidates = pairs
            .checked_add(1)
            .ok_or_else(|| Error::programming("selection candidate count overflow"))?;
        params.push(count_param(candidates as u64)?);
        let limit = params.len();
        params.push(count_param(bytes as u64)?);
        let byte_param = params.len();
        params.push(count_param(pairs as u64)?);
        let pair_param = params.len();
        let clauses = SearchSql {
            joins: Some(&joins),
            predicate: Some(&filter.sql),
            order: Some(&order.sql),
            limit: Some(limit),
            offset: None,
        };
        let sql = erp_sql::select::selection_catalog(
            spec,
            [columns[0], columns[1]],
            &clauses,
            byte_param,
            pair_param,
        );
        tx.run_params(&sql, &params).await
    }

    /// Without `opts.order` the query has
    /// no `ORDER BY`.
    pub async fn search_query(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<SearchQuery> {
        let _search_scope = crate::search_method::scope(tx)?;
        let domain = expand_user_search(tx, self, domain).await?;
        let access = search_access(tx, self, &domain).await?;
        let domain = crate::hierarchy::resolve(tx, self, &domain, true).await?;
        let unaccent = tx.unaccent_status().await?;
        let filter = compile_search(
            tx,
            self,
            &domain,
            access.as_ref(),
            opts.active_test,
            unaccent,
        )
        .await?;
        let order = match opts.order.as_deref() {
            Some(order) => Some(compile_search_order(tx, self, order, filter.params.len()).await?),
            None => None,
        };
        if let Some(order) = &order {
            erp_sql::order::check_join_conflicts(&[&filter.join_list, &order.joins])?;
        }
        Ok(SearchQuery {
            records: self.browse_ids([]),
            filter,
            order,
            limit: opts.limit,
            offset: opts.offset,
            ids: None,
            plan: PlanClass::ByParams,
        })
    }

    /// Everything `search_with` does before the query runs: the access state, the checked
    /// columns to fetch, the compiled query with the model's order when `opts` has none. Sends no
    /// data statement; the query AST's `check` stops here.
    pub(crate) async fn search_plan(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<(SearchQuery, Vec<FieldId>)> {
        // the policy generation and the subject's groups first: every field check below reads the
        // binding, so a membership or policy write of this transaction (or a fresh `with_user`) is
        // seen before the first mask is asked for
        if !self.env().su() {
            tx.prepare_access(self.env()).await?;
        }
        let columns = checked_columns(tx, self.env(), self.spec(), &opts.fetch)?;
        let mut query = self.search_query(tx, domain, opts).await?;
        if query.order.is_none() {
            query.order =
                Some(compile_search_order(tx, self, "", query.filter.params.len()).await?);
        }
        Ok((query, columns))
    }

    /// `search(domain)`: matching records in the model's order (one `SELECT` of ids).
    pub async fn search(&self, tx: &mut Transaction, domain: &Domain) -> Result<Records> {
        self.search_with(tx, domain, &SearchOptions::default())
            .await
    }

    /// Field names are checked even when the domain is `FALSE` (no query then). The result's
    /// prefetch set is its own ids.
    pub async fn search_with(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Records> {
        let (query, columns) = self.search_plan(tx, domain, opts).await?;
        if query.is_empty() {
            return Ok(self.browse_ids([]));
        }
        let ids = query.execute(tx, &columns).await?;
        Ok(self.browse_ids(ids))
    }

    pub async fn search_count(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        limit: Option<u64>,
    ) -> Result<u64> {
        let opts = SearchOptions {
            limit,
            ..SearchOptions::default()
        };
        self.search_query(tx, domain, &opts).await?.count(tx).await
    }

    pub async fn name_search(
        &self,
        tx: &mut Transaction,
        name: &str,
    ) -> Result<Vec<(i64, Option<Arc<str>>)>> {
        self.name_search_with(tx, name, &NameSearch::default())
            .await
    }

    /// `name_search(name, domain, operator, limit)`: records whose `display_name` matches, as
    /// `(id, display_name)` pairs, in the model's order. The rec name is fetched by the search
    /// itself for the built-in fallback. Custom labels use their declared
    /// compute dependencies; contextual or relational inputs can require additional reads.
    pub async fn name_search_with(
        &self,
        tx: &mut Transaction,
        name: &str,
        options: &NameSearch,
    ) -> Result<Vec<(i64, Option<Arc<str>>)>> {
        let condition = Domain::condition("display_name", options.operator, name)?;
        let mut opts = SearchOptions::new().fetch_fields(["display_name"]);
        opts.limit = options.limit;
        let found = self
            .search_with(tx, &(condition & options.domain.clone()), &opts)
            .await?;
        let names = tx
            .display_names_for(&found.engine_sudo(), found.env())
            .await?;
        Ok(found
            .rec_ids()
            .iter()
            .zip(names)
            .map(|(id, cell)| {
                let name = match cell {
                    erp_fields::Cell::Str(s) => Some(s),
                    _ => None,
                };
                (id.get(), name)
            })
            .collect())
    }

    /// `search_read(domain, fields)`: search fetching `fields` (every prefetchable field when
    /// empty), then [`Records::read`] (every field when `fields` is empty).
    pub async fn search_read(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        fields: &[&str],
        opts: &SearchOptions,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        let opts = if fields.is_empty() {
            opts.clone().fetch_prefetchable()
        } else {
            opts.clone().fetch_fields(fields.iter().copied())
        };
        let found = self.search_with(tx, domain, &opts).await?;
        found.read(tx, fields).await
    }

    /// `fetch(field_names)`: make sure the given fields (every prefetchable stored field for
    /// `None`) are cached for these records, querying only what is missing.
    /// Records that no longer exist are skipped, never an error.
    /// The
    /// memo ends on a data statement, a pending change of a field a rule reads, a savepoint
    /// rollback, an invalidation or the end of an isolated scope.
    pub async fn fetch(&self, tx: &mut Transaction, fields: Option<&[&str]>) -> Result<()> {
        let Some(plan) = self.fetch_plan(tx, fields).await? else {
            return Ok(());
        };
        let fetched = plan.run_cached(tx).await?;
        self.fetch_finish(tx, plan, fetched).await
    }

    /// The first half of [`Records::fetch`]: the checks, the access decision and the statements
    /// to send, without sending them. `None` when there is nothing to do. sends the
    /// statements of several independent fetches in one flight ([`fetch_together`]).
    pub(crate) async fn fetch_plan(
        &self,
        tx: &mut Transaction,
        fields: Option<&[&str]>,
    ) -> Result<Option<FetchPlan>> {
        tx.admit_records(self).await?;
        if fields.is_some_and(<[&str]>::is_empty) {
            return Ok(None);
        }
        let spec = self.spec();
        let model = self.model_id();
        let mut seen = FxHashSet::default();
        let ids: Vec<RealId> = self
            .rec_ids()
            .iter()
            .filter_map(|id| id.as_real())
            .filter(|id| seen.insert(*id))
            .collect();
        if ids.is_empty() {
            return Ok(None);
        }
        let graph = tx.shared.graph.model(model);
        let mut wanted = fields_to_fetch(spec, fields, Some(graph))?;
        let probe = wanted.first().copied();
        let env = self.env().clone();
        if !env.su() {
            tx.prepare_access(&env).await?;
            tx.check_model(&env, model, Operation::Read)?;
        }
        match fields {
            // named fields the environment may not read are refused, never skipped
            Some(names) => {
                tx.check_field_names(&env, model, names.iter().copied(), FieldOp::Read)?;
            }
            // the prefetchable set is cut to the readable fields
            None => {
                let mask = tx.field_mask(&env, model)?;
                if !mask.is_all() {
                    wanted.retain(|f| mask.readable(*f));
                }
            }
        }
        let missing: Vec<FieldId> = wanted
            .into_iter()
            .filter(|f| {
                ids.iter()
                    .any(|id| !tx.cache.contains(model, *f, RecId::real(*id)))
            })
            .collect();
        // the access predicate is evaluated for the records the read memo does not know yet
        // (SEC-017: a warm check costs nothing)
        let has_unknown = if env.su() {
            false
        } else {
            tx.has_unknown_read(&env, model, &ids)?
        };
        let restriction = if !has_unknown {
            None
        } else {
            read_restriction(tx, self).await?
        };
        let computed: Vec<FieldId> = fields
            .unwrap_or_default()
            .iter()
            .filter_map(|n| spec.field(n))
            .filter(|f| !f.store && f.is_computed())
            .map(|f| f.id)
            .collect();
        let work = if missing.is_empty() && restriction.is_some() {
            FetchWork::Decide
        } else if !(missing.is_empty() && restriction.is_none()) {
            if let Some(filter) = &restriction {
                flush_read_inputs(tx, &filter.fields).await?;
            }
            let statements = FetchStatements::new(
                tx.registry(),
                spec,
                &missing,
                restriction.as_deref(),
                tx.config().prefetch_chunk,
            );
            FetchWork::Rows {
                missing,
                restriction,
                statements,
            }
        } else {
            FetchWork::Nothing
        };
        Ok(Some(FetchPlan {
            env,
            model,
            ids,
            computed,
            probe,
            work,
        }))
    }

    /// The second half of [`Records::fetch`]: with the ids the plan's statements found (their
    /// rows already cached), settle the access decision and compute what was asked for.
    pub(crate) async fn fetch_finish(
        &self,
        tx: &mut Transaction,
        plan: FetchPlan,
        fetched: Fetched,
    ) -> Result<()> {
        let (surviving, computed) = self.fetch_apply(tx, plan, fetched).await?;
        surviving.compute_fetched(tx, computed).await
    }

    /// Settle the access decision with the ids the plan's statements found (their rows already
    /// cached); the non-stored computed fields still to compute ([`Records::compute_fetched`]).
    pub(crate) async fn fetch_apply(
        &self,
        tx: &mut Transaction,
        plan: FetchPlan,
        fetched: Fetched,
    ) -> Result<(Records, Vec<FieldId>)> {
        let FetchPlan {
            env,
            model,
            ids,
            computed,
            probe,
            work,
        } = plan;
        match work {
            FetchWork::Nothing => {}
            FetchWork::Decide => {
                let all: Vec<RecId> = ids.iter().map(|id| RecId::real(*id)).collect();
                let visible: FxHashSet<RecId> = tx
                    .readable_ids(&env, model, &all)
                    .await?
                    .into_iter()
                    .collect();
                if visible.len() < ids.len() {
                    let hidden: Vec<RealId> = ids
                        .iter()
                        .copied()
                        .filter(|id| !visible.contains(&RecId::real(*id)))
                        .collect();
                    if let Some(error) = tx
                        .record_access_error(&env, model, Operation::Read, &hidden)
                        .await?
                    {
                        return Err(error);
                    }
                }
            }
            FetchWork::Rows {
                missing: _,
                restriction,
                statements: _,
            } => {
                // a decision made with a start set found by a search is never remembered: it
                // depends on data the predicate does not list
                if let Some(filter) = restriction.as_deref().filter(|f| !f.volatile) {
                    tx.remember_read(
                        &env,
                        model,
                        fetched.iter().filter_map(|id| id.as_real()),
                        &filter.fields,
                    )?;
                    // what the query saw is what the rules decide for these records
                    tx.remember_readable(&env, model, &ids, &fetched, &filter.fields)?;
                }
                if restriction.is_some() && fetched.len() < ids.len() {
                    let hidden: Vec<RealId> = ids
                        .iter()
                        .copied()
                        .filter(|id| !fetched.contains(&RecId::real(*id)))
                        .collect();
                    if let Some(error) = tx
                        .record_access_error(&env, model, Operation::Read, &hidden)
                        .await?
                    {
                        return Err(error);
                    }
                }
            }
        }
        let surviving = if let Some(probe_fid) = probe {
            let alive = |id: &RecId| {
                id.is_new() || fetched.contains(id) || tx.cache.contains(model, probe_fid, *id)
            };
            let existing: Vec<RecId> = self.rec_ids().iter().copied().filter(&alive).collect();
            let prefetch: Vec<RecId> = self.prefetch_ids().iter().copied().filter(&alive).collect();
            if existing.len() == self.len() && prefetch.len() == self.prefetch_ids().len() {
                self.clone()
            } else {
                Records::from_parts(self.env().clone(), model, existing.into(), prefetch.into())
            }
        } else {
            self.clone()
        };
        Ok((surviving, computed))
    }

    /// Compute the non-stored computed fields a fetch asked for.
    pub(crate) async fn compute_fetched(
        &self,
        tx: &mut Transaction,
        computed: Vec<FieldId>,
    ) -> Result<()> {
        for field in computed {
            tx.ensure(self, field).await?;
        }
        Ok(())
    }

    /// The result is what searches compile.
    ///
    /// # Errors
    pub fn optimize_domain(
        &self,
        domain: &Domain,
        level: OptimizationLevel,
        now: Option<DateTime>,
    ) -> Result<Domain> {
        self.optimize_with(None, domain, level, now)
    }

    /// [`Records::optimize_domain`] in `tx`, whose access policy resolves `access` conditions
    /// for this environment's principal.
    ///
    /// # Errors
    pub fn optimize_domain_in(
        &self,
        tx: &Transaction,
        domain: &Domain,
        level: OptimizationLevel,
        now: Option<DateTime>,
    ) -> Result<Domain> {
        let _search_scope = crate::search_method::scope(tx)?;
        tx.check_records(self)?;
        let expanded = crate::search_method::expand_search_domains(tx, self, domain)?;
        let lookup = crate::access::TxAccess::new(tx, self.env());
        self.optimize_with(Some(&lookup), &expanded, level, now)
    }

    fn optimize_with(
        &self,
        access: Option<&dyn crate::optimize::AccessLookup>,
        domain: &Domain,
        level: OptimizationLevel,
        now: Option<DateTime>,
    ) -> Result<Domain> {
        let tz = ZoneTz::new(self.env().tz());
        let now = match now {
            Some(now) => primitive_now(now)?,
            None => now_utc()?,
        };
        optimize_domain(
            &OptimizeCtx {
                registry: self.env().registry(),
                tz: &tz,
                now,
                su: self.env().su(),
                access,
            },
            self.spec(),
            domain,
            level,
        )
    }
}

impl<M: Model> RecordSet<M> {
    /// `search(domain)` on the typed surface.
    pub async fn search(&self, tx: &mut Transaction, domain: &Domain) -> Result<Self> {
        Ok(Self::unchecked(self.as_records().search(tx, domain).await?))
    }

    /// `search(domain, offset, limit, order)` / `search_fetch` on the typed surface.
    pub async fn search_with(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        opts: &SearchOptions,
    ) -> Result<Self> {
        Ok(Self::unchecked(
            self.as_records().search_with(tx, domain, opts).await?,
        ))
    }

    /// `search_count(domain, limit)` on the typed surface.
    pub async fn search_count(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        limit: Option<u64>,
    ) -> Result<u64> {
        self.as_records().search_count(tx, domain, limit).await
    }

    pub async fn name_search(
        &self,
        tx: &mut Transaction,
        name: &str,
    ) -> Result<Vec<(i64, Option<Arc<str>>)>> {
        self.as_records().name_search(tx, name).await
    }

    /// `name_search(name, domain, operator, limit)` on the typed surface.
    pub async fn name_search_with(
        &self,
        tx: &mut Transaction,
        name: &str,
        options: &NameSearch,
    ) -> Result<Vec<(i64, Option<Arc<str>>)>> {
        self.as_records().name_search_with(tx, name, options).await
    }

    /// `search_read(domain, fields)` on the typed surface.
    pub async fn search_read(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        fields: &[&str],
        opts: &SearchOptions,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        self.as_records()
            .search_read(tx, domain, fields, opts)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounded memory: never more than `capacity` entries; an entry used between rotations survives.
    #[test]
    fn two_generation_eviction_keeps_hot_entries() {
        let mut memo: TwoGen<u32, u32> = TwoGen::new(4);
        memo.insert(0, 0);
        for i in 1..100 {
            memo.insert(i, i);
            assert!(memo.len() <= 4, "{} entries", memo.len());
            assert_eq!(memo.get(&0), Some(0), "hot key evicted at {i}");
        }
        assert_eq!(memo.get(&1), None, "cold keys are evicted");
        let mut off: TwoGen<u32, u32> = TwoGen::new(0);
        off.insert(1, 1);
        assert_eq!((off.get(&1), off.len()), (None, 0));
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;
    use bytes::BytesMut;
    use erp_fields::{Cell, Storage, TimestampTz, Uuid, Xid8};
    use postgres_types::FromSql;

    fn round_trip<T>(scalar: Scalar, cell: Cell, storage: Storage, expected: T)
    where
        T: for<'a> FromSql<'a> + Clone + PartialEq + std::fmt::Debug,
    {
        let param = Param::Scalar(scalar.clone());
        let native = param_type(&param);
        let bound = bind(&[param]).unwrap();
        let mut bytes = BytesMut::new();
        bound[0].to_sql_checked(&native, &mut bytes).unwrap();
        assert_eq!(T::from_sql(&native, &bytes).unwrap(), expected);
        assert!(
            bound[0]
                .to_sql_checked(&Type::TEXT, &mut BytesMut::new())
                .is_err()
        );
        let array = Param::Array(scalar.kind(), vec![scalar]);
        let array_type = param_type(&array);
        let bound = bind(&[array]).unwrap();
        let mut bytes = BytesMut::new();
        bound[0].to_sql_checked(&array_type, &mut bytes).unwrap();
        assert_eq!(
            Vec::<T>::from_sql(&array_type, &bytes).unwrap(),
            vec![expected.clone()]
        );
        let optional = crate::write::array_param(storage, vec![cell, Cell::Null]).unwrap();
        let mut bytes = BytesMut::new();
        optional.to_sql_checked(&array_type, &mut bytes).unwrap();
        let decoded = Vec::<Option<T>>::from_sql(&array_type, &bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].as_ref(), Some(&expected));
        assert!(decoded[1].is_none());
    }

    #[test]
    fn native_fields_bind_native_scalar_and_nullable_write_array_types() {
        round_trip(
            Scalar::Uuid(Uuid::NIL),
            Cell::Uuid(Uuid::NIL),
            Storage::Uuid,
            Uuid::NIL,
        );
        let high = Xid8::from_u64(u64::MAX);
        round_trip(Scalar::Xid8(high), Cell::Xid8(high), Storage::Xid8, high);
        let time = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
        round_trip(
            Scalar::TimestampTz(time),
            Cell::TimestampTz(time),
            Storage::TimestampTz,
            time,
        );
        let mixed = Param::Array(ScalarKind::Xid8, vec![Scalar::Int8(1)]);
        let error = bind(&[mixed]).expect_err("mixed array rejected");
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), "array parameter with mixed value types");
    }
}

#[cfg(test)]
mod catalog_property_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    #[test]
    fn catalog_properties_refuse_before_implicit_default_preparation() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "catalog_property_guard",
            depends: &[],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("catalog_property_guard", "catalog.property")
            .fields(
                const {
                    &[
                        FieldDecl::integer("plain"),
                        FieldDecl::char("note").company_dependent(true),
                    ]
                },
            );
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let model = registry.model("catalog.property").unwrap();
        assert!(
            refuse_catalog_properties(&registry, &[(model.id, model.field("plain").unwrap().id)])
                .is_ok()
        );
        let error =
            refuse_catalog_properties(&registry, &[(model.id, model.field("note").unwrap().id)])
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "catalog.property.note: company-dependent selection catalog expressions is not supported until a later release"
        );
    }
}
