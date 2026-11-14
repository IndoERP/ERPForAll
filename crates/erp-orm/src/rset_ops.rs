//! Recordset operations that need the transaction (RSET §4.7–4.13): `exists`, row locks,
//! `filtered`, `filtered_domain`, `mapped` over paths, `sorted` and `grouped`.
//!
//!   Every other operation is final.
//! - Everything is column-wise (RSET W1/W2): a field is loaded once for the whole recordset (one
//!   prefetch query at most, none when cached) and the operation is one pass over the column.
//! - `exists` asks the database every time with one `= ANY($1)` statement per chunk
//!   of ids, without access, `active` or flush, and leaves the cache alone.
//! - Locks never wait: `SKIP LOCKED`, so the transaction is not aborted and needs no savepoint;
//!   a row that cannot be locked is a `Lock` error (409) from `lock_for_update` and is skipped by
//!   `try_lock_for_update`.
//! - `sorted` parses orders with the grammar `search` uses (`erp_sql::order::parse_order`) and
//!   places NULLs with the same PostgreSQL defaults.
//!   Each term is turned into dense ranks once
//!   (a many2one into the ranks of its distinct targets under the comodel's order, recursively),
//!   then rows are sorted on rank tuples with a stable sort and no `id` tiebreaker. Every column
//!   is checked against its field type first: a cached value of another kind is a `Programming`
//!   error, never a silent tie.
//!
//! Relational paths (`filtered('parent_id.state_id')`, `mapped('parent_id.name')`) walk one field
//! per step over the union of the previous step's targets (`Records::related`), so each step costs
//! one prefetch query at most whatever the number of records. Grouping by a relational field gives
//! recordset keys ([`Records::grouped_records`]).

use std::any::Any;
use std::cmp::Ordering;
use std::hash::Hash;
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Cell, Error, ErrorKind, FieldType, FieldValue, RealId, RecId, Result, Value};
use erp_registry::{FieldId, ModelId, ModelSpec};
use erp_sql::order::{Nulls, parse_order};
use erp_sql::rows::{LockMode, existing_ids, lock_ids, lock_ids_ordered};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_postgres::Row;

use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::{Field, Inherits, Model, RecordSet, Records};

fn invalid_field(spec: &ModelSpec, name: &str) -> Error {
    Error::programming(format!("Invalid field '{name}' on '{}'", spec.name))
}

fn no_table(spec: &ModelSpec) -> Error {
    Error::programming(format!(
        "The abstract model {} has no table, so it has no records",
        spec.name
    ))
}

fn lock_error() -> Error {
    Error::new(ErrorKind::Lock, "Cannot grab a lock on records")
}

fn decode_id(row: &Row) -> Result<RecId> {
    let raw: i64 = row
        .try_get(0)
        .map_err(|e| Error::new(ErrorKind::Database, "could not decode an id").with_source(e))?;
    RecId::from_real(raw)
        .ok_or_else(|| Error::programming(format!("the database returned the id {raw}")))
}

/// The distinct database ids of `records`, in first-occurrence order.
fn distinct_real(records: &Records) -> Vec<RealId> {
    let mut seen = FxHashSet::default();
    records
        .rec_ids()
        .iter()
        .filter_map(|id| id.as_real())
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Row indices as `u32` (sort and filter work on them).
fn row_indices(n: usize) -> Result<Vec<u32>> {
    u32::try_from(n)
        .map(|n| (0..n).collect())
        .map_err(|_| Error::programming("recordset operations support at most 2^32 records"))
}

// ----- exists and row locks ------------------------------------------------------------------------

/// `base.exists`: see the module documentation.
async fn exists_query(tx: &mut Transaction, records: &Records) -> Result<Records> {
    tx.admit_records(records).await?;
    let real = distinct_real(records);
    if real.is_empty() {
        return Ok(records.clone());
    }
    let spec = records.spec();
    if !spec.has_table() {
        return Err(no_table(spec));
    }
    let sql = existing_ids(&spec.table);
    let mut found: FxHashSet<RecId> = FxHashSet::default();
    for chunk in real.chunks(tx.config().prefetch_chunk.max(1)) {
        let param = crate::write::id_param(chunk);
        for row in &tx.query(&sql, &[&param]).await? {
            found.insert(decode_id(row)?);
        }
    }
    // new records exist by convention
    Ok(records.browse_ids(
        records
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| id.is_new() || found.contains(id)),
    ))
}

/// Lock what can be locked among `ids` (distinct; all of them, or the first `limit` in order);
/// the ids locked. With `all_or_nothing`, the chunks after the first one that could not lock all
/// its rows are not sent (`lock_for_update` fails anyway).
async fn lock_rows(
    tx: &mut Transaction,
    spec: &ModelSpec,
    ids: &[RealId],
    mode: LockMode,
    limit: Option<u64>,
    all_or_nothing: bool,
) -> Result<FxHashSet<RecId>> {
    tx.require_writable(crate::WriteOp::Lock, spec.name)?;
    let mut locked = FxHashSet::default();
    let chunk = tx.config().prefetch_chunk.max(1);
    match limit {
        None => {
            let sql = lock_ids(&spec.table, mode);
            for part in ids.chunks(chunk) {
                let param = crate::write::id_param(part);
                let rows = tx.query(&sql, &[&param]).await?;
                for row in &rows {
                    locked.insert(decode_id(row)?);
                }
                if all_or_nothing && rows.len() < part.len() {
                    break;
                }
            }
        }
        Some(mut remaining) => {
            let sql = lock_ids_ordered(&spec.table, mode);
            for part in ids.chunks(chunk) {
                if remaining == 0 {
                    break;
                }
                let param = crate::write::id_param(part);
                let limit = i64::try_from(remaining)
                    .map_err(|_| Error::programming(format!("limit out of range: {remaining}")))?;
                for row in &tx.query(&sql, &[&param, &limit]).await? {
                    if locked.insert(decode_id(row)?) {
                        remaining = remaining.saturating_sub(1);
                    }
                }
            }
        }
    }
    Ok(locked)
}

impl Records {
    /// `exists()`: the records whose row exists, in order with duplicates; new records always
    /// count as existing. One query per chunk of distinct ids, none without
    /// database ids; no access rule, `active` filter or flush; the cache is neither read nor
    /// changed. The result's prefetch set is its own ids. Runs the model's
    /// `exists` override chain.
    pub async fn exists(&self, tx: &mut Transaction) -> Result<Records> {
        call_method::<(), Records>(tx, self, "exists", ()).await
    }

    /// `lock_for_update(allow_referencing=…)`: lock every row, or fail with `Lock` ("Cannot grab
    /// a lock on records", HTTP 409) when one is locked by another transaction or does not exist
    /// (/026). Never waits; re-locking rows this transaction holds succeeds; new
    /// records are ignored and no query runs without database ids. `allow_referencing` takes the
    /// weaker `FOR NO KEY UPDATE` lock, which lets other transactions insert rows referencing these
    /// records. Rows locked before the error stay locked until the transaction ends.
    pub async fn lock_for_update(
        &self,
        tx: &mut Transaction,
        allow_referencing: bool,
    ) -> Result<()> {
        tx.admit_records(self).await?;
        let ids = distinct_real(self);
        if ids.is_empty() {
            return Ok(());
        }
        let spec = self.spec();
        if !spec.has_table() {
            return Err(no_table(spec));
        }
        let locked =
            lock_rows(tx, spec, &ids, LockMode::new(allow_referencing), None, true).await?;
        if locked.len() == ids.len() {
            Ok(())
        } else {
            Err(lock_error())
        }
    }

    /// `try_lock_for_update(allow_referencing=…, limit=…)`: the records that could be locked, in
    /// order with duplicates, skipping rows other transactions hold (/028). New records
    /// count as locked and come first against `limit`: with at least `limit` of them, the first
    /// `limit` new records are returned without a query; otherwise the database ids are locked in
    /// recordset order, at most `limit` minus the new records.
    pub async fn try_lock_for_update(
        &self,
        tx: &mut Transaction,
        allow_referencing: bool,
        limit: Option<u64>,
    ) -> Result<Records> {
        tx.admit_records(self).await?;
        let new_ids: Vec<RecId> = self
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| id.is_new())
            .collect();
        let mut remaining = None;
        if let Some(limit) = limit {
            let new_count = new_ids.len() as u64;
            if new_count >= limit {
                let take = usize::try_from(limit).unwrap_or(usize::MAX);
                return Ok(self.browse_ids(new_ids.into_iter().take(take)));
            }
            remaining = Some(limit - new_count);
        }
        let ids = distinct_real(self);
        if ids.is_empty() {
            return Ok(self.clone());
        }
        let spec = self.spec();
        if !spec.has_table() {
            return Err(no_table(spec));
        }
        let locked = lock_rows(
            tx,
            spec,
            &ids,
            LockMode::new(allow_referencing),
            remaining,
            false,
        )
        .await?;
        Ok(self.browse_ids(
            self.rec_ids()
                .iter()
                .copied()
                .filter(|id| id.is_new() || locked.contains(id)),
        ))
    }
}

// ----- filtered, filtered_domain, mapped ---------------------------------------------------------

fn check_path(records: &Records, operation: &str, path: &str) -> Result<()> {
    let registry = records.env().registry();
    let mut model = records.spec();
    let mut names = path.split('.').peekable();
    while let Some(name) = names.next() {
        let field = model
            .field(name)
            .ok_or_else(|| invalid_field(model, name))?;
        if names.peek().is_none() {
            break;
        }
        model = field
            .ty
            .comodel()
            .and_then(|c| registry.model(c))
            .ok_or_else(|| {
                Error::programming(format!(
                    "{operation}('{path}'): {}.{} is not a relational field, so '{path}' is not a \
                     path",
                    model.name, field.name
                ))
            })?;
    }
    Ok(())
}

impl Records {
    fn path_truthy<'a>(
        &'a self,
        tx: &'a mut Transaction,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Vec<bool>>> {
        Box::pin(async move {
            let Some((head, rest)) = path.split_once('.') else {
                let values = self.values(tx, path).await?;
                return Ok(values.iter().map(Value::is_truthy).collect());
            };
            let field = self
                .spec()
                .field(head)
                .ok_or_else(|| invalid_field(self.spec(), head))?;
            let targets = self.related(tx, head).await?;
            let cells = tx.field_cells(self, field.id).await?;
            let truthy = targets.path_truthy(tx, rest).await?;
            let good: FxHashSet<RecId> = targets
                .rec_ids()
                .iter()
                .zip(truthy)
                .filter_map(|(id, t)| t.then_some(*id))
                .collect();
            Ok(cells
                .iter()
                .map(|cell| match cell {
                    Cell::Id(i) => RecId::from_real(*i).is_some_and(|r| good.contains(&r)),
                    Cell::Ids(ids) => ids.iter().any(|r| good.contains(r)),
                    _ => false,
                })
                .collect())
        })
    }

    /// `mapped(path)` for a checked, non-empty path.
    fn mapped_steps<'a>(
        &'a self,
        tx: &'a mut Transaction,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Mapped>> {
        Box::pin(async move {
            match path.split_once('.') {
                Some((head, rest)) => {
                    let targets = self.related(tx, head).await?;
                    targets.mapped_steps(tx, rest).await
                }
                None => {
                    let relational = self
                        .spec()
                        .field(path)
                        .is_some_and(|f| f.ty.comodel().is_some());
                    Ok(if relational {
                        Mapped::Records(self.related(tx, path).await?)
                    } else {
                        Mapped::Values(self.values(tx, path).await?)
                    })
                }
            }
        })
    }
}

impl Records {
    /// `filtered_domain(domain)`: the records satisfying `domain`, evaluated in memory, in order
    /// with duplicates, sharing the prefetch set. An empty recordset
    /// or the `TRUE` domain returns the recordset unchanged. Runs the model's `filtered_domain`
    /// override chain; evaluated in memory matching database search semantics.
    pub async fn filtered_domain(&self, tx: &mut Transaction, domain: &Domain) -> Result<Records> {
        call_method::<(Domain,), Records>(tx, self, "filtered_domain", (domain.clone(),)).await
    }

    /// An empty path returns the recordset unchanged. Each step of a
    /// path reads one field for the union of the previous step's targets. Unknown fields are an
    /// error.
    pub async fn filtered_path(&self, tx: &mut Transaction, path: &str) -> Result<Records> {
        if path.is_empty() {
            return Ok(self.clone());
        }
        check_path(self, "filtered", path)?;
        if !path.contains('.') {
            return self.filtered_values(tx, path, Value::is_truthy).await;
        }
        let truthy = self.path_truthy(tx, path).await?;
        let kept: Vec<RecId> = self
            .rec_ids()
            .iter()
            .zip(&truthy)
            .filter_map(|(id, t)| t.then_some(*id))
            .collect();
        Ok(if kept.len() == self.len() {
            self.clone()
        } else {
            self.derive(kept)
        })
    }

    /// The records for which `predicate` holds on the record-format value of `field` (one column
    /// read, one pass), in order with duplicates, sharing the prefetch set.
    pub async fn filtered_values(
        &self,
        tx: &mut Transaction,
        field: &str,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Result<Records> {
        let values = self.values(tx, field).await?;
        let kept: Vec<RecId> = self
            .rec_ids()
            .iter()
            .zip(&values)
            .filter(|(_, v)| predicate(v))
            .map(|(id, _)| *id)
            .collect();
        Ok(if kept.len() == self.len() {
            self.clone()
        } else {
            self.derive(kept)
        })
    }

    /// `filtered(callable)` with a callable that needs no database access: `predicate` sees each
    /// record as a singleton sharing this set's prefetch set. Use [`Records::filtered_values`] or
    /// [`RecordSet::filtered`] to filter on field values.
    #[must_use]
    pub fn filtered_by(&self, mut predicate: impl FnMut(&Records) -> bool) -> Records {
        let kept: Vec<RecId> = self
            .iter()
            .filter(|r| predicate(r))
            .map(|r| r.rec_ids()[0])
            .collect();
        self.derive(kept)
    }

    /// Each
    /// step reads one field for the whole previous recordset. Unknown names are an error.
    pub async fn mapped_path(&self, tx: &mut Transaction, path: &str) -> Result<Mapped> {
        if path.is_empty() {
            return Ok(Mapped::Records(self.clone()));
        }
        check_path(self, "mapped", path)?;
        self.mapped_steps(tx, path).await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    /// A recordset (an empty path, or a path ending on a relational field).
    Records(Records),
    /// One value per record of the last recordset.
    Values(Vec<Value>),
}

impl Mapped {
    /// The values, when the path ended on a non-relational field.
    pub fn into_values(self) -> Option<Vec<Value>> {
        match self {
            Self::Values(values) => Some(values),
            Self::Records(_) => None,
        }
    }

    /// The recordset, when the path was empty or ended on a relational field.
    pub fn into_records(self) -> Option<Records> {
        match self {
            Self::Records(records) => Some(records),
            Self::Values(_) => None,
        }
    }
}

// ----- grouped -------------------------------------------------------------------------------------

/// Partition row positions by key, keys in first-occurrence order.
fn group_positions<K: Eq + Hash>(keys: impl IntoIterator<Item = K>) -> Vec<(K, Vec<usize>)> {
    let mut index: FxHashMap<K, usize> = FxHashMap::default();
    let mut members: Vec<Vec<usize>> = Vec::new();
    for (row, key) in keys.into_iter().enumerate() {
        let next = members.len();
        let slot = *index.entry(key).or_insert(next);
        if slot == next {
            members.push(Vec::new());
        }
        members[slot].push(row);
    }
    let mut keyed: Vec<(K, usize)> = index.into_iter().collect();
    keyed.sort_unstable_by_key(|(_, slot)| *slot);
    keyed
        .into_iter()
        .zip(members)
        .map(|((key, _), rows)| (key, rows))
        .collect()
}

impl Records {
    /// `grouped('field')`: the records partitioned by the record-format value of `field`, keys in
    /// first-occurrence order, each group in this set's order with duplicates and sharing this
    /// set's prefetch set, so a field loaded through one group is loaded for all.
    /// A dotted name is not a path: it is an unknown field. An empty recordset
    /// gives no groups and reads nothing. No aggregation (see `read_group`).
    pub async fn grouped(
        &self,
        tx: &mut Transaction,
        field: &str,
    ) -> Result<Vec<(Value, Records)>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let values = self.values(tx, field).await?;
        Ok(self.groups(group_positions(values)))
    }

    /// One column read.
    pub async fn grouped_records(
        &self,
        tx: &mut Transaction,
        field: &str,
    ) -> Result<Vec<(Records, Records)>> {
        let spec = self.spec();
        let fs = spec
            .field(field)
            .ok_or_else(|| invalid_field(spec, field))?;
        let comodel = fs
            .ty
            .comodel()
            .and_then(|c| self.env().registry().model_id(c))
            .ok_or_else(|| {
                Error::programming(format!(
                    "grouped_records('{field}'): {}.{field} is not a relational field",
                    spec.name
                ))
            })?;
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let cells = tx.field_cells(self, fs.id).await?;
        let keys: Vec<Vec<RecId>> = cells
            .iter()
            .map(|cell| match cell {
                Cell::Id(i) => RecId::from_real(*i).into_iter().collect(),
                Cell::Ids(ids) => {
                    let mut key = ids.to_vec();
                    key.sort_unstable();
                    key.dedup();
                    key
                }
                _ => Vec::new(),
            })
            .collect();
        let env = self.env().clone();
        Ok(self
            .groups(group_positions(keys))
            .into_iter()
            .map(|(key, records)| {
                let ids: Arc<[RecId]> = Arc::from(key);
                (
                    Records::from_parts(env.clone(), comodel, Arc::clone(&ids), ids),
                    records,
                )
            })
            .collect())
    }

    fn groups<K>(&self, groups: Vec<(K, Vec<usize>)>) -> Vec<(K, Records)> {
        let ids = self.rec_ids();
        groups
            .into_iter()
            .map(|(key, rows)| (key, self.derive(rows.into_iter().map(|r| ids[r]).collect())))
            .collect()
    }
}

// ----- sorted --------------------------------------------------------------------------------------

/// Dense ranks of `n` rows under `cmp`: equal rows share a rank, ranks ascend in `cmp` order.
fn dense_ranks(n: usize, cmp: impl Fn(usize, usize) -> Ordering) -> Result<Vec<u32>> {
    let mut order = row_indices(n)?;
    order.sort_unstable_by(|a, b| cmp(*a as usize, *b as usize));
    let mut ranks = vec![0u32; n];
    let mut rank = 0u32;
    for k in 0..order.len() {
        let row = order[k] as usize;
        if k > 0 && cmp(order[k - 1] as usize, row) != Ordering::Equal {
            rank += 1;
        }
        ranks[row] = rank;
    }
    Ok(ranks)
}

fn record_cell(ty: &FieldType, cell: Cell) -> Cell {
    match (ty, cell) {
        (FieldType::Boolean, Cell::Null) => Cell::Bool(false),
        (FieldType::Integer, Cell::Null) => Cell::Int(0),
        (FieldType::Float { digits: None }, Cell::Null) => Cell::Float(0.0),
        (FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. }, Cell::Null) => {
            Cell::Decimal(erp_fields::Decimal::ZERO)
        }
        (_, cell) => cell,
    }
}

/// Ordering of two non-NULL cells of one column: text in code-point order (the `C` collation),
/// `-0.0` equal to `0.0` (as SQL compares floats; values are finite).
fn compare_cells(a: &Cell, b: &Cell) -> Ordering {
    match (a, b) {
        (Cell::Bool(a), Cell::Bool(b)) => a.cmp(b),
        (Cell::Int(a), Cell::Int(b)) => a.cmp(b),
        (Cell::Id(a), Cell::Id(b)) => a.cmp(b),
        (Cell::Float(a), Cell::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Cell::Decimal(a), Cell::Decimal(b)) => a.cmp(b),
        (Cell::Str(a), Cell::Str(b)) => a.cmp(b),
        (Cell::Date(a), Cell::Date(b)) => a.cmp(b),
        (Cell::DateTime(a), Cell::DateTime(b)) => a.cmp(b),
        (Cell::Uuid(a), Cell::Uuid(b)) => a.cmp(b),
        (Cell::BigInt(a), Cell::BigInt(b)) => a.cmp(b),
        (Cell::Xid8(a), Cell::Xid8(b)) => a.cmp(b),
        (Cell::TimestampTz(a), Cell::TimestampTz(b)) => a.cmp(b),
        // PostgreSQL's range order: by lower end (unbounded first), then upper end (unbounded last)
        (Cell::DateRange(a), Cell::DateRange(b)) => compare_ranges(a, b),
        (Cell::DatetimeRange(a), Cell::DatetimeRange(b)) => compare_ranges(a, b),
        // never reached: every column is checked against its field type before ranking
        // (`predicate::check_column`), which reports a mismatch as a Programming error
        _ => Ordering::Equal,
    }
}

/// Two ranges of one field (same bounds) in PostgreSQL's order: lower ends first, an unbounded
/// lower end before any value; then upper ends, an unbounded upper end after any value.
fn compare_ranges<T: Copy + Ord>(a: &erp_fields::Range<T>, b: &erp_fields::Range<T>) -> Ordering {
    let upper = |r: &erp_fields::Range<T>| (r.upper().is_none(), r.upper());
    a.lower()
        .cmp(&b.lower())
        .then_with(|| upper(a).cmp(&upper(b)))
}

/// Ranks of a term over `keys` (`None` is NULL), with the term's direction and NULL placement
/// folded in so that ascending ranks are the wanted order.
fn term_ranks<T>(
    keys: &[Option<T>],
    descending: bool,
    nulls_first: bool,
    cmp: impl Fn(&T, &T) -> Ordering,
) -> Result<Vec<u32>> {
    dense_ranks(keys.len(), |i, j| match (&keys[i], &keys[j]) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(a), Some(b)) => {
            let o = cmp(a, b);
            if descending { o.reverse() } else { o }
        }
    })
}

/// Lexicographic comparison of rows over per-term rank columns.
fn compare_rows(terms: &[Vec<u32>], a: usize, b: usize) -> Ordering {
    for ranks in terms {
        match ranks[a].cmp(&ranks[b]) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// With `null_row`, each
/// column has one more row after the records: a virtual record whose every value is NULL, the
/// row a LEFT JOIN produces for a many2one without target, so that such records sort where SQL
/// puts them under the comodel order (first when it starts with a descending term).
fn order_ranks<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    order: &'a str,
    expanding: &'a mut Vec<(ModelId, FieldId)>,
    null_row: bool,
) -> BoxFuture<'a, Result<Vec<Vec<u32>>>> {
    Box::pin(async move {
        let invalid = || Error::programming(format!("Invalid order '{order}' to sort"));
        let terms = parse_order(order).ok_or_else(invalid)?;
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let rows = records.len() + usize::from(null_row);
        let mut columns = Vec::with_capacity(terms.len());
        for term in terms {
            let field = spec
                .field(term.field)
                .ok_or_else(|| invalid_field(spec, term.field))?;
            let descending = term.direction.descending();
            let nulls_first = term.nulls.first_for(term.direction);
            let relational_error = || {
                let part = match term.property {
                    Some(p) => format!("{}.{p}", term.field),
                    None => term.field.to_owned(),
                };
                Error::programming(format!(
                    "Invalid order on relational field '{part}' to sort"
                ))
            };
            // the `:func` part of a term is accepted by the grammar and ignored (RSET Q5)
            let ranks = match (&field.ty, term.property) {
                (FieldType::Many2one { .. }, Some("id")) => {
                    let cells = tx.field_cells(records, field.id).await?;
                    crate::predicate::check_column(spec, field, &cells)?;
                    let mut keys: Vec<Option<i64>> = cells
                        .iter()
                        .map(|c| match c {
                            Cell::Id(i) => Some(*i),
                            _ => None,
                        })
                        .collect();
                    keys.extend(null_row.then_some(None));
                    term_ranks(&keys, descending, nulls_first, Ord::cmp)?
                }
                (FieldType::Many2one { comodel, .. }, None) => {
                    let key = (spec.id, field.id);
                    if expanding.contains(&key) {
                        vec![0; rows]
                    } else {
                        let cells = tx.field_cells(records, field.id).await?;
                        crate::predicate::check_column(spec, field, &cells)?;
                        let mut targets: Vec<Option<RecId>> = cells
                            .iter()
                            .map(|c| match c {
                                Cell::Id(i) => RecId::from_real(*i),
                                _ => None,
                            })
                            .collect();
                        targets.extend(null_row.then_some(None));
                        expanding.push(key);
                        let ranked = comodel_ranks(tx, records, comodel, &targets, expanding).await;
                        expanding.pop();
                        let (ranked, null_rank) = ranked?;
                        // SQL: an explicit NULLS clause adds `f IS [NOT] NULL` before the comodel
                        // order; without one, records without target sort as the all-NULL
                        // comodel row
                        let explicit = term.nulls != Nulls::Default;
                        let keys: Vec<Option<u32>> = targets
                            .iter()
                            .map(|t| match t {
                                Some(t) => ranked.get(t).copied(),
                                None if explicit => None,
                                None => Some(null_rank),
                            })
                            .collect();
                        term_ranks(&keys, descending, nulls_first, Ord::cmp)?
                    }
                }
                (FieldType::Many2one { .. }, Some(_)) => return Err(relational_error()),
                (FieldType::Id, None) => {
                    let mut keys: Vec<Option<i64>> =
                        records.rec_ids().iter().map(|id| Some(id.get())).collect();
                    keys.extend(null_row.then_some(None));
                    term_ranks(&keys, descending, nulls_first, Ord::cmp)?
                }
                (FieldType::Json, _) => {
                    return Err(Error::programming(format!(
                        "Cannot sort by the JSON field {}.{}: its values have no order",
                        spec.name, field.name
                    )));
                }
                (ty, _)
                    if !matches!(ty, FieldType::Boolean)
                        && erp_sql::param::ScalarKind::of_field(ty).is_none() =>
                {
                    return Err(relational_error());
                }
                (_, Some(property)) => return Err(property_error(spec, field.name, property)),
                (ty, None) => {
                    let cells = tx.field_cells(records, field.id).await?;
                    crate::predicate::check_column(spec, field, &cells)?;
                    let mut keys: Vec<Option<Cell>> = cells
                        .into_iter()
                        .map(|c| match record_cell(ty, c) {
                            Cell::Null => None,
                            c => Some(c),
                        })
                        .collect();
                    // the joined NULL row: NULL in SQL, except booleans (`COALESCE(f, FALSE)`)
                    keys.extend(
                        null_row
                            .then(|| matches!(ty, FieldType::Boolean).then_some(Cell::Bool(false))),
                    );
                    term_ranks(&keys, descending, nulls_first, compare_cells)?
                }
            };
            columns.push(ranks);
        }
        Ok(columns)
    })
}

/// The model's `_order`, `id` when it has none.
fn model_order(spec: &ModelSpec) -> &'static str {
    if spec.order.trim().is_empty() {
        "id"
    } else {
        spec.order
    }
}

fn property_error(spec: &ModelSpec, field: &str, property: &str) -> Error {
    Error::programming(format!(
        "Sorting by the property '{property}' of {}.{field} is not supported until a later \
         milestone",
        spec.name
    ))
}

/// The rank of each distinct target under the comodel's `_order` (read in the caller's
/// environment, one prefetch query for the targets), and the rank of "no target": the all-NULL
/// row of SQL's LEFT JOIN.
async fn comodel_ranks(
    tx: &mut Transaction,
    records: &Records,
    comodel: &str,
    targets: &[Option<RecId>],
    expanding: &mut Vec<(ModelId, FieldId)>,
) -> Result<(FxHashMap<RecId, u32>, u32)> {
    let mut seen = FxHashSet::default();
    let distinct: Vec<RecId> = targets
        .iter()
        .flatten()
        .copied()
        .filter(|t| seen.insert(*t))
        .collect();
    let comodel_records = records.env().records(comodel)?.browse_ids(distinct);
    let order = model_order(comodel_records.spec());
    let terms = order_ranks(tx, &comodel_records, order, expanding, true).await?;
    let n = comodel_records.len();
    let ranks = dense_ranks(n + 1, |a, b| compare_rows(&terms, a, b))?;
    let null_rank = ranks[n];
    Ok((
        comodel_records
            .rec_ids()
            .iter()
            .copied()
            .zip(ranks)
            .collect(),
        null_rank,
    ))
}

impl Records {
    /// `sorted(order, reverse)`: the same records ordered by an order specification (the model's
    /// `_order` for `None`), sharing the prefetch set. Stable: records equal on every term keep
    /// their input order, also with `reverse`. No `id` tiebreaker is added (applies
    /// to `search` only); write `…, id` to order ties by id. Fewer than two records are returned unchanged
    /// without looking at `order`. Errors: an order that does not parse is
    /// "Invalid order '…' to sort", an unknown field is an invalid field, a one2many/many2many or a
    /// many2one with a property other than `id` is "Invalid order on relational field '…' to
    /// sort". See the module documentation for the comparison rules.
    pub async fn sorted(
        &self,
        tx: &mut Transaction,
        order: Option<&str>,
        reverse: bool,
    ) -> Result<Records> {
        if self.len() < 2 {
            return Ok(self.clone());
        }
        tx.admit_records(self).await?;
        let order = order.unwrap_or_else(|| model_order(self.spec()));
        let mut expanding = Vec::new();
        let terms = order_ranks(tx, self, order, &mut expanding, false).await?;
        let mut rows = row_indices(self.len())?;
        if reverse {
            rows.sort_by(|a, b| compare_rows(&terms, *b as usize, *a as usize));
        } else {
            rows.sort_by(|a, b| compare_rows(&terms, *a as usize, *b as usize));
        }
        let ids = self.rec_ids();
        Ok(self.derive(rows.into_iter().map(|r| ids[r as usize]).collect()))
    }

    /// `sorted(key=callable)` with a key that needs no database access: a stable sort of the
    /// records (as singletons) by `key`; `reverse` keeps equal records in input order.
    #[must_use]
    pub fn sorted_by_key<K: Ord>(
        &self,
        mut key: impl FnMut(&Records) -> K,
        reverse: bool,
    ) -> Records {
        let mut keyed: Vec<(K, RecId)> = self.iter().map(|r| (key(&r), r.rec_ids()[0])).collect();
        if reverse {
            keyed.sort_by(|a, b| b.0.cmp(&a.0));
        } else {
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
        }
        self.derive(keyed.into_iter().map(|(_, id)| id).collect())
    }
}

// ----- typed surface -------------------------------------------------------------------------------

impl<M: Model> RecordSet<M> {
    /// `exists()` (see [`Records::exists`]).
    pub async fn exists(&self, tx: &mut Transaction) -> Result<Self> {
        Ok(Self::unchecked(self.as_records().exists(tx).await?))
    }

    /// `try_lock_for_update()` (see [`Records::try_lock_for_update`]).
    pub async fn try_lock_for_update(
        &self,
        tx: &mut Transaction,
        allow_referencing: bool,
        limit: Option<u64>,
    ) -> Result<Self> {
        Ok(Self::unchecked(
            self.as_records()
                .try_lock_for_update(tx, allow_referencing, limit)
                .await?,
        ))
    }

    /// `filtered_domain(domain)` (see [`Records::filtered_domain`]).
    pub async fn filtered_domain(&self, tx: &mut Transaction, domain: &Domain) -> Result<Self> {
        Ok(Self::unchecked(
            self.as_records().filtered_domain(tx, domain).await?,
        ))
    }

    /// `filtered(lambda r: predicate(r.field))`: one column read for the whole set, then one pass;
    /// in order with duplicates, sharing the prefetch set.
    pub async fn filtered<P: Model, T: FieldValue>(
        &self,
        tx: &mut Transaction,
        field: Field<P, T>,
        mut predicate: impl FnMut(&T) -> bool,
    ) -> Result<Self>
    where
        M: Inherits<P>,
    {
        let values = self.mapped(tx, field).await?;
        let kept: Vec<RecId> = self
            .rec_ids()
            .iter()
            .zip(&values)
            .filter(|(_, v)| predicate(v))
            .map(|(id, _)| *id)
            .collect();
        Ok(Self::unchecked(self.as_records().derive(kept)))
    }

    /// `filtered(callable)` with a callable that needs no database access (see
    /// [`Records::filtered_by`]).
    #[must_use]
    pub fn filtered_by(&self, mut predicate: impl FnMut(&Self) -> bool) -> Self {
        Self::unchecked(
            self.as_records()
                .filtered_by(|r| predicate(&Self::unchecked(r.clone()))),
        )
    }

    /// `sorted(order, reverse)` (see [`Records::sorted`]).
    pub async fn sorted(
        &self,
        tx: &mut Transaction,
        order: Option<&str>,
        reverse: bool,
    ) -> Result<Self> {
        Ok(Self::unchecked(
            self.as_records().sorted(tx, order, reverse).await?,
        ))
    }

    /// `sorted(key=callable, reverse)` (see [`Records::sorted_by_key`]).
    #[must_use]
    pub fn sorted_by_key<K: Ord>(&self, mut key: impl FnMut(&Self) -> K, reverse: bool) -> Self {
        Self::unchecked(
            self.as_records()
                .sorted_by_key(|r| key(&Self::unchecked(r.clone())), reverse),
        )
    }

    /// `grouped('field')` with typed keys (see [`Records::grouped`]).
    pub async fn grouped<P: Model, T: FieldValue + Eq + Hash>(
        &self,
        tx: &mut Transaction,
        field: Field<P, T>,
    ) -> Result<Vec<(T, Self)>>
    where
        M: Inherits<P>,
    {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let values = self.mapped(tx, field).await?;
        Ok(self
            .as_records()
            .groups(group_positions(values))
            .into_iter()
            .map(|(k, r)| (k, Self::unchecked(r)))
            .collect())
    }

    /// `grouped(callable)` with a key that needs no database access: groups in first-occurrence
    /// order, sharing the prefetch set.
    #[must_use]
    pub fn grouped_by<K: Eq + Hash>(&self, mut key: impl FnMut(&Self) -> K) -> Vec<(K, Self)> {
        let keys: Vec<K> = self.iter().map(|r| key(&r)).collect();
        self.as_records()
            .groups(group_positions(keys))
            .into_iter()
            .map(|(k, r)| (k, Self::unchecked(r)))
            .collect()
    }
}

// ----- base implementations of the overridable methods --------------------------------------------

type ExistsFn = MethodFn<(), Records>;
type FilteredDomainFn = MethodFn<(Domain,), Records>;

fn base_exists<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    _sup: Super<(), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(exists_query(tx, records))
}

fn base_filtered_domain<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Domain,),
    _sup: Super<(Domain,), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(async move { crate::predicate::filter_records(tx, records, &args.0).await })
}

fn exists_entry() -> &'static (dyn Any + Send + Sync) {
    static F: ExistsFn = base_exists;
    &F
}

fn filtered_domain_entry() -> &'static (dyn Any + Send + Sync) {
    static F: FilteredDomainFn = base_filtered_domain;
    &F
}

/// The `base` implementations of `exists` and `filtered_domain`, registered by
/// [`crate::Dispatch::build`] like the CRUD built-ins (a `linkme` element in this crate would need
/// `#[link_section]`).
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_EXISTS, &BASE_FILTERED_DOMAIN];

static BASE_EXISTS: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "exists",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<ExistsFn>,
    entry: exists_entry,
    file: file!(),
    line: line!(),
};

static BASE_FILTERED_DOMAIN: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "filtered_domain",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<FilteredDomainFn>,
    entry: filtered_domain_entry,
    file: file!(),
    line: line!(),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_order_like_postgresql() {
        use erp_fields::{Date, DateRange};
        let d = |n: u32| Some(Date::from_ymd(2026, 1, n).unwrap());
        let mut cells = vec![
            Cell::DateRange(DateRange::new(d(2), None)),
            Cell::DateRange(DateRange::new(d(2), d(5))),
            Cell::DateRange(DateRange::new(None, d(9))),
            Cell::DateRange(DateRange::new(d(1), d(3))),
        ];
        cells.sort_by(compare_cells);
        assert_eq!(
            cells,
            [
                Cell::DateRange(DateRange::new(None, d(9))),
                Cell::DateRange(DateRange::new(d(1), d(3))),
                Cell::DateRange(DateRange::new(d(2), d(5))),
                Cell::DateRange(DateRange::new(d(2), None)),
            ]
        );
    }

    #[test]
    fn groups_keep_first_occurrence_order() {
        let groups = group_positions(["b", "a", "b", "c", "a"]);
        assert_eq!(
            groups,
            vec![("b", vec![0, 2]), ("a", vec![1, 4]), ("c", vec![3])]
        );
    }

    #[test]
    fn term_ranks_fold_direction_and_nulls() {
        let keys = [Some(2), None, Some(1), Some(2)];
        // ASC, nulls last (default)
        assert_eq!(
            term_ranks(&keys, false, false, Ord::cmp).unwrap(),
            [1, 2, 0, 1]
        );
        // DESC, nulls first (default)
        assert_eq!(
            term_ranks(&keys, true, true, Ord::cmp).unwrap(),
            [1, 0, 2, 1]
        );
        // ASC NULLS FIRST
        assert_eq!(
            term_ranks(&keys, false, true, Ord::cmp).unwrap(),
            [2, 0, 1, 2]
        );
    }

    #[test]
    fn record_format_nulls() {
        assert_eq!(record_cell(&FieldType::Integer, Cell::Null), Cell::Int(0));
        assert_eq!(
            record_cell(&FieldType::Boolean, Cell::Null),
            Cell::Bool(false)
        );
        assert_eq!(record_cell(&FieldType::Text, Cell::Null), Cell::Null);
        assert_eq!(
            compare_cells(&Cell::Str("é".into()), &Cell::Str("z".into())),
            Ordering::Greater,
            "code-point order"
        );
        assert_eq!(
            compare_cells(&Cell::Str("B".into()), &Cell::Str("a".into())),
            Ordering::Less,
            "C collation"
        );
        assert_eq!(
            compare_cells(&Cell::Float(-0.0), &Cell::Float(0.0)),
            Ordering::Equal,
            "-0.0 ties with 0.0 as in SQL"
        );
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};
    #[test]
    fn native_fields_sort_with_native_scalar_order() {
        for (lower, upper) in [
            (
                Cell::Uuid(Uuid::NIL),
                Cell::Uuid(Uuid::from_bytes([255; 16])),
            ),
            (
                Cell::Xid8(Xid8::from_u64(9)),
                Cell::Xid8(Xid8::from_u64(u64::MAX)),
            ),
            (
                Cell::TimestampTz(TimestampTz::UNIX_EPOCH),
                Cell::TimestampTz(TimestampTz::from_unix_micros(1).unwrap()),
            ),
        ] {
            assert_eq!(compare_cells(&lower, &upper), Ordering::Less);
            assert_eq!(compare_cells(&upper, &lower), Ordering::Greater);
        }
    }
}
