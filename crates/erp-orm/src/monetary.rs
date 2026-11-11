//! Operation-local currency resolution and native rounding.

use std::{
    borrow::Cow,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use erp_fields::{Cell, Decimal, Error, FieldType, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::FxHashMap;

use crate::{Env, Records, Transaction};

/// The task that owns async candidate currency computes; named by every refusal that points at
/// the `candidate` flag.
const ASYNC_CURRENCY_TASK: &str = "a later release";

/// The refusal of an async currency compute that is not registered `candidate`: the message
/// names the flag as the fix and the task ("Async candidate currency computes").
pub(crate) fn async_currency_refusal(what: &str, model: &str, field: &str, when: &str) -> Error {
    Error::programming(format!(
        "Cannot {what} async monetary currency {model}.{field} {when}: declare its compute \
         `candidate` (#[compute(candidate, ...)], a read-only async compute) to join same-call \
         monetary admission ({ASYNC_CURRENCY_TASK})"
    ))
}

/// The result of a candidate compute that admission ran for a write, waiting to be published
/// with it (see [`Transaction::candidate_stage`]).
pub(crate) struct StagedCandidate {
    pub(crate) model: ModelId,
    pub(crate) field: FieldId,
    pub(crate) ids: Vec<RecId>,
    pub(crate) out: crate::compute::Outputs,
}

/// Collects the candidate results of one admission and restores the outer collector when it
/// ends, also when the write future is dropped mid-admission (cancellation), so a cancelled
/// candidate can never be reused by a later recompute.
pub(crate) struct CandidateScope<'a> {
    tx: &'a mut Transaction,
    outer: Option<Option<Vec<StagedCandidate>>>,
}

impl<'a> CandidateScope<'a> {
    pub(crate) fn open(tx: &'a mut Transaction) -> Self {
        let outer = tx.candidate_stage.replace(Vec::new());
        Self {
            tx,
            outer: Some(outer),
        }
    }

    /// End the scope and hand back what the admission collected.
    pub(crate) fn finish(mut self) -> Option<Vec<StagedCandidate>> {
        let outer = self.outer.take().unwrap_or(None);
        std::mem::replace(&mut self.tx.candidate_stage, outer)
    }
}

impl std::ops::Deref for CandidateScope<'_> {
    type Target = Transaction;
    fn deref(&self) -> &Transaction {
        self.tx
    }
}

impl std::ops::DerefMut for CandidateScope<'_> {
    fn deref_mut(&mut self) -> &mut Transaction {
        self.tx
    }
}

impl Drop for CandidateScope<'_> {
    fn drop(&mut self) {
        if let Some(outer) = self.outer.take() {
            self.tx.candidate_stage = outer;
        }
    }
}

/// Maximum staged monetary cells admitted per operation.
pub(crate) const MAX_STAGED_CELLS: usize = 2_000_000;

/// Maximum target records admitted per monetary operation.
pub(crate) const MAX_TARGET_RECORDS: usize = 200_000;

/// The reason a monetary operation was refused before staging. Internal: callers see an
/// [`Error`] of kind `Validation` whose message names the limit and the task, never an input
/// count (the oversized request is the caller's input, like the record caps of `copy` and
/// the web save).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    /// Staged monetary cells or target records exceeded the operation bound.
    MonetaryStagingExceeded {
        /// Number of target records in the operation.
        records: usize,
        /// Total staged monetary cells including the refused request (`usize::MAX` on overflow).
        cells: usize,
        /// The cell limit.
        max_cells: usize,
        /// The record limit.
        max_records: usize,
    },
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MonetaryStagingExceeded {
                cells,
                max_cells,
                max_records,
                ..
            } => {
                // When both limits are exceeded the cell limit is the one named.
                if *cells > *max_cells {
                    write!(
                        f,
                        "monetary staging cells limit ({max_cells}) exceeded in task a later release"
                    )
                } else {
                    write!(
                        f,
                        "monetary staging target records limit ({max_records}) exceeded in task a later release"
                    )
                }
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

impl From<AdmissionError> for Error {
    fn from(err: AdmissionError) -> Self {
        Error::validation(err.to_string()).with_source(err)
    }
}

/// One operation's admission budget (one `write`, `create`, `onchange` or default-fill call).
/// It is created before any staging buffer, shared by every chunk, same-call dependency
/// projection and precompute of that call, and dropped with the call. Each staged cell is charged
/// once per buffer that holds it. The record bound applies to the operation's target records
/// the first time a monetary cell would be staged; it is not charged again per chunk.
pub(crate) struct StagingBudget {
    records: usize,
    cells: AtomicUsize,
    max_records: usize,
    max_cells: usize,
    /// Projection cells already charged, per field, as a bitset over the operation's row
    /// positions: a same-call projection charges each record-field once however many amounts
    /// share the computed currency and however many paths stage the same input.
    projected: Mutex<FxHashMap<FieldId, Vec<u64>>>,
}

impl StagingBudget {
    /// A budget for an operation over `records` target records.
    pub(crate) fn new(records: usize) -> Self {
        Self::with_limits(records, MAX_TARGET_RECORDS, MAX_STAGED_CELLS)
    }

    pub(crate) fn with_limits(records: usize, max_records: usize, max_cells: usize) -> Self {
        Self {
            records,
            cells: AtomicUsize::new(0),
            max_records,
            max_cells,
            projected: Mutex::new(FxHashMap::default()),
        }
    }

    /// Refuse an operation whose target records exceed the bound, whatever it stages.
    pub(crate) fn check_records(&self) -> Result<()> {
        if self.records > self.max_records {
            return Err(self.refuse(self.cells.load(Ordering::Relaxed)));
        }
        Ok(())
    }

    fn projected_lock(&self) -> std::sync::MutexGuard<'_, FxHashMap<FieldId, Vec<u64>>> {
        // the set is only ever extended; a poisoned lock still holds a valid set
        self.projected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many of the cells of `field` at `positions` no projection has charged yet.
    pub(crate) fn unseen_projected(&self, field: FieldId, positions: &[usize]) -> usize {
        let guard = self.projected_lock();
        let bits = guard.get(&field);
        positions
            .iter()
            .filter(|p| {
                !bits.is_some_and(|b| b.get(**p / 64).is_some_and(|w| w & (1 << (**p % 64)) != 0))
            })
            .count()
    }

    /// Charge the cells of `field` at `positions` that no projection charged yet.
    pub(crate) fn charge_projected(&self, field: FieldId, positions: &[usize]) -> Result<()> {
        self.check(self.unseen_projected(field, positions))?;
        let mut fresh = 0usize;
        {
            let mut guard = self.projected_lock();
            let bits = guard.entry(field).or_default();
            for &p in positions {
                let word = p / 64;
                if bits.len() <= word {
                    bits.resize(word + 1, 0);
                }
                if bits[word] & (1 << (p % 64)) == 0 {
                    bits[word] |= 1 << (p % 64);
                    fresh += 1;
                }
            }
        }
        self.cells.fetch_add(fresh, Ordering::Relaxed);
        Ok(())
    }

    fn refuse(&self, cells: usize) -> Error {
        AdmissionError::MonetaryStagingExceeded {
            records: self.records,
            cells,
            max_cells: self.max_cells,
            max_records: self.max_records,
        }
        .into()
    }

    /// Refuse when `extra` more cells would exceed a bound; records nothing.
    pub(crate) fn check(&self, extra: usize) -> Result<()> {
        if extra == 0 {
            return Ok(());
        }
        let total = self
            .cells
            .load(Ordering::Relaxed)
            .checked_add(extra)
            .ok_or_else(|| self.refuse(usize::MAX))?;
        if self.records > self.max_records || total > self.max_cells {
            return Err(self.refuse(total));
        }
        Ok(())
    }

    /// [`Self::check`] for `rows` rows of up to `per_row` cells each, with checked arithmetic.
    pub(crate) fn check_grid(&self, rows: usize, per_row: usize) -> Result<()> {
        let extra = rows
            .checked_mul(per_row)
            .ok_or_else(|| self.refuse(usize::MAX))?;
        self.check(extra)
    }

    /// Admit and record `extra` staged cells.
    pub(crate) fn charge(&self, extra: usize) -> Result<()> {
        self.check(extra)?;
        self.cells.fetch_add(extra, Ordering::Relaxed);
        Ok(())
    }
}

/// The compact slot list of one operation's staging buffers: the candidate monetary fields, their
/// currency fields, and everything a currency or a same-call dependency projection can read or
/// resolve from them (compute inputs and groups, related hops, `_inherits` delegates, and the
/// currencies of monetary inputs). Sorted by field id; buffers are as wide as this list, never as
/// wide as the model.
pub(crate) fn staging_slots(
    spec: &ModelSpec,
    graph: &crate::graph::ModelGraph,
    monetary: &[FieldId],
) -> Vec<FieldId> {
    let mut slots: Vec<FieldId> = Vec::new();
    let mut todo: Vec<FieldId> = monetary.to_vec();
    while let Some(field) = todo.pop() {
        if slots.contains(&field) {
            continue;
        }
        slots.push(field);
        let fs = spec.field_by_id(field);
        if let FieldType::Monetary {
            currency_field: Some(name),
        } = &fs.ty
            && let Some(currency) = spec.field_id(name)
        {
            todo.push(currency);
        }
        if let Some(inherited) = fs.inherited {
            todo.push(inherited.delegate);
        }
        if let Some(fc) = graph.compute(field) {
            todo.extend(fc.readable.iter().copied());
            todo.extend(fc.group.iter().copied());
            todo.extend(fc.depends.iter().copied());
            todo.extend(fc.normalization.iter().copied());
            match &fc.source {
                crate::graph::Source::Related(target) => todo.push(*target),
                crate::graph::Source::RelatedPath(path) => todo.push(path[0].1),
                _ => {}
            }
        }
    }
    slots.sort();
    slots
}

/// A compact staged group, indexed by row and candidate field slot. No values are published here.
pub(crate) struct Stage<'a> {
    /// Origin-free new-record fill without cached plain values.
    pub(crate) initial: bool,
    pub(crate) spec: &'a ModelSpec,
    pub(crate) ids: Option<&'a [RecId]>,
    /// The slot list: column `k` of every row (and of `included`) holds field `fields[k]`.
    /// Sorted by field id, as [`staging_slots`] returns it.
    pub(crate) fields: &'a [FieldId],
    pub(crate) rows: &'a mut [Vec<Option<Cell>>],
    pub(crate) included: Option<&'a [Vec<bool>]>,
    /// The operation's budget, charged for the cells this stage holds.
    pub(crate) budget: &'a StagingBudget,
    /// Cells (`[row][slot]`) an earlier round of the same operation already charged.
    pub(crate) precharged: Option<&'a [Vec<bool>]>,
    pub(crate) kind: StageKind<'a>,
}

/// How the budget is charged for a stage.
#[derive(Clone, Copy)]
pub(crate) enum StageKind<'a> {
    /// Staged by the operation itself: every included cell not yet charged counts.
    Direct,
    /// Staged again by a same-call dependency projection: each record-field counts once per
    /// operation. `positions[row]` is the row's position in the operation's own stage (`None`:
    /// the rows are that stage).
    Projection { positions: Option<&'a [usize]> },
}

struct Inputs<'a> {
    /// Authorization of the values' producer, retained across nested compute readers.
    producer: &'a Env,
    /// Elevate only currency identity traversal, never a native compute's inputs.
    currency_lookup: bool,
    initial: bool,
    env: &'a Env,
    spec: &'a ModelSpec,
    ids: Option<&'a [RecId]>,
    fields: &'a [FieldId],
    rows: &'a [Vec<Option<Cell>>],
    active: &'a [bool],
    included: Option<&'a [Vec<bool>]>,
    budget: &'a StagingBudget,
    /// Position of each row in the operation's own stage (`None`: identity).
    positions: Option<&'a [usize]>,
}

impl Inputs<'_> {
    fn global(&self, row: usize) -> usize {
        self.positions.map_or(row, |p| p[row])
    }

    fn find_slot(&self, field: FieldId) -> Option<usize> {
        self.fields.binary_search(&field).ok()
    }

    /// The column of `field`. A field the operation reads or resolves is part of the closure
    /// [`staging_slots`] builds, so a missing one is a programming error, never "not staged".
    fn slot_of(&self, field: FieldId) -> Result<usize> {
        self.find_slot(field).ok_or_else(|| {
            Error::programming(format!(
                "Monetary staging has no slot for {}.{}",
                self.spec.name,
                self.spec.field_by_id(field).name
            ))
        })
    }
    fn resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        mut field: FieldId,
    ) -> crate::dispatch::BoxFuture<'a, Result<Vec<Option<Cell>>>> {
        Box::pin(async move {
            enum Step {
                SameRecord {
                    target: FieldId,
                    env: Env,
                    bound: Option<FieldId>,
                },
                Inherited {
                    field: erp_registry::Inherited,
                    env: Env,
                },
                Related {
                    path: crate::graph::Path,
                    env: Env,
                    bound: Option<FieldId>,
                },
            }
            // A model has a finite field set. Each step visits a new field or refuses;
            // continuations are heap-backed, so a long related chain does not grow the stack.
            let mut trail = Vec::new();
            let mut frames: Vec<(Vec<Option<Cell>>, Step)> = Vec::new();
            let mut answer;
            let mut run_env = self.env.clone();
            let mut bound_field = None;
            let mut starts_cache: Option<Vec<Option<RecId>>> = None;
            loop {
                let slot = self.slot_of(field)?;
                let given: Vec<Option<Cell>> = self
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        if !self.active[i] {
                            Some(Cell::Null)
                        } else {
                            r[slot].clone()
                        }
                    })
                    .collect();
                let read_env = if self.currency_lookup {
                    run_env.sudo(true)
                } else {
                    run_env.clone()
                };
                tx.check_field(
                    &read_env,
                    self.spec.id,
                    field,
                    crate::field_access::FieldOp::Read,
                )?;
                if given.iter().all(Option::is_some) {
                    answer = given;
                    break;
                }
                if trail.contains(&field) {
                    return Err(Error::programming(format!(
                        "Monetary currency resolution cycle at {}.{}",
                        self.spec.name,
                        self.spec.field_by_id(field).name
                    )));
                }
                trail.push(field);
                let fs = self.spec.field_by_id(field);
                let shared = Arc::clone(&tx.shared);
                if let Some(inherited) = fs.inherited {
                    frames.push((
                        given,
                        Step::Inherited {
                            field: inherited,
                            env: run_env.clone(),
                        },
                    ));
                    field = inherited.delegate;
                    continue;
                }
                if let Some(fc) = shared.graph.model(self.spec.id).compute(field) {
                    // The exposed field is checked above as its reader. Following its
                    // related path retains the declared compute environment and company bound.
                    if bound_field.is_none()
                        && crate::related_company::Bound::of(self.env, self.spec.id, fs).is_some()
                    {
                        bound_field = Some(field);
                    }
                    let related_env = if fc.sudo {
                        run_env.sudo(true)
                    } else {
                        run_env.clone()
                    };
                    match &fc.source {
                        crate::graph::Source::Related(target) => {
                            frames.push((
                                given,
                                Step::SameRecord {
                                    target: *target,
                                    env: related_env.clone(),
                                    bound: bound_field,
                                },
                            ));
                            run_env = related_env;
                            field = *target;
                            continue;
                        }
                        crate::graph::Source::RelatedPath(path) => {
                            frames.push((
                                given,
                                Step::Related {
                                    path: Arc::clone(path),
                                    env: related_env.clone(),
                                    bound: bound_field,
                                },
                            ));
                            run_env = related_env;
                            field = path[0].1;
                            continue;
                        }
                        _ => {}
                    }
                    for readable in fc.readable.iter() {
                        self.slot_of(*readable)?;
                    }
                    if let Some(&amount) = fc.readable.iter().find(|amount| {
                        let needs_visited_currency = match &self.spec.field_by_id(**amount).ty {
                            FieldType::Monetary {
                                currency_field: Some(name),
                            } => self
                                .spec
                                .field_id(name)
                                .is_some_and(|currency| trail.contains(&currency)),
                            // Only monetary inputs can depend on currency normalization.
                            _ => false,
                        };
                        let slot = self.find_slot(**amount);
                        !fc.group.contains(amount)
                            && needs_visited_currency
                            && self.rows.iter().enumerate().any(|(i, row)| {
                                given[i].is_none()
                                    && self.active[i]
                                    && slot.is_some_and(|idx| row[idx].is_some())
                            })
                    }) {
                        return Err(Error::programming(format!(
                            "Monetary currency resolution cycle: {}.{} requires {} before normalization",
                            self.spec.name,
                            fs.name,
                            self.spec.field_by_id(amount).name
                        )));
                    }
                    if self.ids.is_none() {
                        answer = given;
                        break;
                    }
                    if matches!(fc.source, crate::graph::Source::Chain(_)) {
                        let changed = self.candidate_changes(tx, &related_env, fc, &given).await?;
                        if changed.iter().any(|changed| *changed) {
                            let project_given: Vec<Option<Cell>> = given
                                .iter()
                                .enumerate()
                                .map(|(i, cell)| {
                                    if changed[i] {
                                        cell.clone()
                                    } else {
                                        Some(Cell::Null)
                                    }
                                })
                                .collect();
                            let projected = self
                                .project(tx, &related_env, field, fc, &project_given)
                                .await?;
                            let old_given: Vec<Option<Cell>> = given
                                .iter()
                                .enumerate()
                                .map(|(i, cell)| {
                                    if changed[i] {
                                        Some(Cell::Null)
                                    } else {
                                        cell.clone()
                                    }
                                })
                                .collect();
                            let old = if old_given.iter().any(Option::is_none) {
                                self.read_resolved(tx, &run_env, field, &old_given).await?
                            } else {
                                vec![None; given.len()]
                            };
                            answer = given
                                .into_iter()
                                .enumerate()
                                .map(|(i, cell)| {
                                    cell.or_else(|| {
                                        if changed[i] {
                                            projected[i].clone()
                                        } else {
                                            old[i].clone()
                                        }
                                    })
                                })
                                .collect();
                            break;
                        }
                    }
                }
                let fallback = if self.ids.is_some() {
                    self.read_resolved(tx, &run_env, field, &given).await?
                } else {
                    vec![Some(Cell::Null); self.rows.len()]
                };
                answer = given
                    .into_iter()
                    .zip(fallback)
                    .map(|(given, fallback)| given.or(fallback))
                    .collect();
                break;
            }
            while let Some((given, step)) = frames.pop() {
                // A missing computed hop is pending, not a resolved empty relation.
                let unresolved: Vec<bool> = answer.iter().map(Option::is_none).collect();
                let fallback = match step {
                    Step::SameRecord { target, env, bound } => {
                        if let Some(bound) = bound.and_then(|field| {
                            crate::related_company::Bound::of(
                                self.env,
                                self.spec.id,
                                self.spec.field_by_id(field),
                            )
                        }) {
                            let starts = if let Some(starts) = &starts_cache {
                                starts.clone()
                            } else {
                                let starts = self.starts(tx)?;
                                starts_cache = Some(starts.clone());
                                starts
                            };
                            let mut hops: Vec<Option<RecId>> = answer
                                .iter()
                                .map(|c| c.as_ref().and_then(crate::relational::id_of))
                                .collect();
                            tx.check_related_hop(
                                &env,
                                bound,
                                (self.spec.id, target),
                                &starts,
                                &mut hops,
                                self.env.is_direct(),
                            )
                            .await?;
                        }
                        answer
                    }
                    Step::Inherited {
                        field: inherited,
                        env,
                    } => {
                        let starts: Vec<Option<RecId>> = answer
                            .iter()
                            .map(|c| c.as_ref().and_then(crate::relational::id_of))
                            .collect();
                        let values = if self.currency_lookup {
                            tx.currency_cells_by_id(
                                &env,
                                (inherited.parent, inherited.parent_field),
                                &starts,
                            )
                            .await?
                        } else {
                            tx.cells_by_id(
                                &env,
                                (inherited.parent, inherited.parent_field),
                                &starts,
                            )
                            .await?
                        };
                        starts
                            .into_iter()
                            .map(|id| match id {
                                None => Ok(Some(Cell::Null)),
                                Some(id) => values.get(&id).cloned().map(Some).ok_or_else(|| {
                                    Error::programming(
                                        "Inherited monetary currency read lost a record",
                                    )
                                }),
                            })
                            .collect::<Result<Vec<_>>>()?
                    }
                    Step::Related { path, env, bound } => {
                        let mut hops: Vec<Option<RecId>> = answer
                            .iter()
                            .map(|c| c.as_ref().and_then(crate::relational::id_of))
                            .collect();
                        let bound = bound.and_then(|field| {
                            crate::related_company::Bound::of(
                                self.env,
                                self.spec.id,
                                self.spec.field_by_id(field),
                            )
                        });
                        let starts = if bound.is_some() {
                            if let Some(starts) = &starts_cache {
                                starts.clone()
                            } else {
                                let starts = self.starts(tx)?;
                                starts_cache = Some(starts.clone());
                                starts
                            }
                        } else {
                            hops.clone()
                        };
                        if let Some(bound) = bound {
                            tx.check_related_hop(
                                &env,
                                bound,
                                path[0],
                                &starts,
                                &mut hops,
                                self.env.is_direct(),
                            )
                            .await?;
                        }
                        let values = if self.currency_lookup {
                            tx.currency_related_from_as(
                                &env,
                                &path[1..],
                                hops,
                                &starts,
                                false,
                                bound,
                            )
                            .await?
                        } else {
                            tx.related_from_as(&env, &path[1..], hops, &starts, false, bound)
                                .await?
                        };
                        values.into_iter().map(Some).collect()
                    }
                };
                answer = given
                    .into_iter()
                    .zip(fallback)
                    .enumerate()
                    .map(|(i, (given, fallback))| {
                        given.or_else(|| if unresolved[i] { None } else { fallback })
                    })
                    .collect();
            }
            Ok(answer)
        })
    }

    /// Match actual notification semantics: reads alone do not invalidate, and
    /// values equal after monetary conversion do not notify. Keep the decision per row.
    async fn candidate_changes(
        &self,
        tx: &mut Transaction,
        env: &Env,
        fc: &crate::graph::FieldCompute,
        given: &[Option<Cell>],
    ) -> Result<Vec<bool>> {
        let wanted = |i: usize| self.active[i] && given[i].is_none();
        if self.initial {
            return Ok((0..self.rows.len()).map(wanted).collect());
        }
        let shared = Arc::clone(&tx.shared);
        let mut todo: Vec<FieldId> = fc
            .depends
            .iter()
            .copied()
            .filter(|f| !fc.group.contains(f))
            .collect();
        let mut fields = Vec::new();
        while let Some(field) = todo.pop() {
            if fields.contains(&field) {
                continue;
            }
            fields.push(field);
            if let Some(next) = shared.graph.model(self.spec.id).compute(field) {
                todo.extend(
                    next.depends
                        .iter()
                        .copied()
                        .filter(|f| !next.group.contains(f)),
                );
            }
            if let Some(inherited) = self.spec.field_by_id(field).inherited {
                todo.push(inherited.delegate);
            }
        }
        let mut candidates: Cow<'_, [Vec<Option<Cell>>]> = Cow::Borrowed(self.rows);
        let candidate_monetary: Vec<FieldId> = fields
            .iter()
            .copied()
            .filter(|f| {
                matches!(self.spec.field_by_id(*f).ty, FieldType::Monetary { .. })
                    && self.find_slot(*f).is_some_and(|idx| {
                        self.rows
                            .iter()
                            .enumerate()
                            .any(|(i, row)| wanted(i) && row[idx].is_some())
                    })
            })
            .collect();
        if !candidate_monetary.is_empty() {
            let width = self.fields.len();
            let included =
                |i: usize, idx: usize| wanted(i) && self.included.is_none_or(|mask| mask[i][idx]);
            // Refuse before the candidate copy of the rows and its mask are allocated.
            let mut cells = 0usize;
            for &f in &candidate_monetary {
                let idx = self.slot_of(f)?;
                let positions: Vec<usize> = self
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(i, row)| included(*i, idx) && row[idx].is_some())
                    .map(|(i, _)| self.global(i))
                    .collect();
                cells = cells.saturating_add(self.budget.unseen_projected(f, &positions));
            }
            self.budget.check(cells)?;
            let mask: Vec<Vec<bool>> = self
                .rows
                .iter()
                .enumerate()
                .map(|(i, _)| (0..width).map(|idx| included(i, idx)).collect())
                .collect();
            normalize(
                tx,
                self.producer,
                Stage {
                    initial: false,
                    spec: self.spec,
                    ids: self.ids,
                    fields: self.fields,
                    rows: candidates.to_mut(),
                    included: Some(&mask),
                    budget: self.budget,
                    precharged: None,
                    kind: StageKind::Projection {
                        positions: self.positions,
                    },
                },
                &candidate_monetary,
                false,
                true,
            )
            .await?;
        }
        let mut changed = vec![false; self.rows.len()];
        for field in fields {
            let idx = self.slot_of(field)?;
            if !candidates
                .iter()
                .enumerate()
                .any(|(i, row)| wanted(i) && row[idx].is_some())
            {
                continue;
            }
            let old = self.read(tx, env, field, given).await?;
            for (i, row) in candidates.iter().enumerate() {
                if wanted(i)
                    && row[idx].as_ref().is_some_and(|candidate| {
                        old[i].as_ref().is_none_or(|old| !old.same_value(candidate))
                    })
                {
                    changed[i] = true;
                }
            }
        }
        Ok(changed)
    }

    /// Execute a sync compute with an operation-local input batch. The output never
    /// enters the transaction cache: ordinary write/recompute retains that ownership.
    async fn project(
        &self,
        tx: &mut Transaction,
        env: &Env,
        field: FieldId,
        fc: &crate::graph::FieldCompute,
        given: &[Option<Cell>],
    ) -> Result<Vec<Option<Cell>>> {
        let crate::graph::Source::Chain(chain) = &fc.source else {
            return Err(Error::programming(
                "Monetary projection requires a compute chain",
            ));
        };
        if fc.is_async && !fc.candidate {
            return Err(async_currency_refusal(
                "project same-call",
                self.spec.name,
                self.spec.field_by_id(field).name,
                "from changed inputs",
            ));
        }
        let ids = self
            .ids
            .ok_or_else(|| Error::programming("Monetary projection has no record ids"))?;
        let positions: Vec<usize> = self
            .active
            .iter()
            .enumerate()
            .filter_map(|(i, active)| (*active && given[i].is_none()).then_some(i))
            .collect();
        let mut result = vec![None; self.rows.len()];
        for chunk in positions.chunks(tx.config().recompute_batch.max(1)) {
            let ids: Arc<[RecId]> = chunk.iter().map(|i| ids[*i]).collect::<Vec<_>>().into();
            let records = Records::from_parts(
                env.clone(),
                self.spec.id,
                Arc::clone(&ids),
                Arc::clone(&ids),
            );
            let mut rows: Vec<Vec<Option<Cell>>> =
                chunk.iter().map(|i| self.rows[*i].clone()).collect();
            let included: Option<Vec<Vec<bool>>> = self
                .included
                .map(|mask| chunk.iter().map(|i| mask[*i].clone()).collect());
            let positions: Vec<usize> = chunk.iter().map(|i| self.global(*i)).collect();
            let readable: Vec<FieldId> = fc
                .readable
                .iter()
                .copied()
                .filter(|f| !fc.group.contains(f))
                .collect();
            let readable_monetary: Vec<FieldId> = readable
                .iter()
                .copied()
                .filter(|f| matches!(self.spec.field_by_id(*f).ty, FieldType::Monetary { .. }))
                .collect();
            if !readable_monetary.is_empty() {
                normalize(
                    tx,
                    self.producer,
                    Stage {
                        initial: self.initial,
                        spec: self.spec,
                        ids: Some(&ids),
                        fields: self.fields,
                        rows: &mut rows,
                        included: included.as_deref(),
                        budget: self.budget,
                        precharged: None,
                        kind: StageKind::Projection {
                            positions: Some(&positions),
                        },
                    },
                    &readable_monetary,
                    false,
                    true,
                )
                .await?;
            }
            let active = vec![true; rows.len()];
            let candidate = Inputs {
                producer: self.producer,
                currency_lookup: false,
                initial: self.initial,
                env,
                spec: self.spec,
                ids: Some(&ids),
                fields: self.fields,
                rows: &rows,
                active: &active,
                included: included.as_deref(),
                budget: self.budget,
                positions: Some(&positions),
            };
            let mut inputs = vec![None; self.spec.fields.len()];
            for &input in &readable {
                inputs[input.index()] = Some(
                    candidate
                        .resolve(tx, input)
                        .await?
                        .into_iter()
                        .map(|cell| {
                            cell.ok_or_else(|| {
                                Error::programming("Unresolved monetary compute input")
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            if !self.initial {
                tx.load_compute_old_values(&records, fc).await?;
            }
            for &input in fc.readable.iter().filter(|input| fc.group.contains(input)) {
                let slot = self.slot_of(input)?;
                inputs[input.index()] = Some(if self.initial {
                    let default = crate::write::default_cell(
                        &records,
                        self.spec,
                        self.spec.field_by_id(input),
                    )?
                    .unwrap_or(Cell::Null);
                    rows.iter()
                        .map(|row| row[slot].clone().unwrap_or_else(|| default.clone()))
                        .collect()
                } else {
                    tx.load_own(&records, input).await?;
                    tx.cells_of(env, self.spec.id, input, &ids)?
                });
            }
            let old = if fc.stored {
                inputs[field.index()].clone()
            } else {
                None
            };
            let outputs = if fc.candidate {
                // Memoized per operation: a second amount resolving this currency must not run
                // the candidate again for the same records.
                let known = tx
                    .candidate_stage
                    .as_ref()
                    .and_then(|stage| {
                        stage
                            .iter()
                            .find(|staged| {
                                staged.model == self.spec.id
                                    && staged.field == field
                                    && staged.ids.as_slice() == &ids[..]
                            })
                            .map(|staged| {
                                staged.out.validate_virtual(tx, &ids)?;
                                // The candidate was admitted before insertion into candidate_stage.
                                Ok::<_, Error>(staged.out.column(field))
                            })
                    })
                    .transpose()?;
                if let Some(column) = known {
                    column
                } else {
                    let columns: Vec<(FieldId, Vec<Cell>)> = fc
                        .readable
                        .iter()
                        .map(|&input| {
                            inputs[input.index()]
                                .clone()
                                .map(|column| (input, column))
                                .ok_or_else(|| {
                                    Error::programming(format!(
                                        "Unresolved candidate compute input {}.{}",
                                        self.spec.name,
                                        self.spec.field_by_id(input).name
                                    ))
                                })
                        })
                        .collect::<Result<_>>()?;
                    let scope = crate::candidate::ReadScope::staged(
                        self.spec.id,
                        Arc::clone(&ids),
                        columns,
                    );
                    let mut out = crate::compute::Outputs::new(
                        env.clone(),
                        self.spec.id,
                        fc.method,
                        &fc.group,
                        ids.len(),
                    );
                    // Membership plus per-row validation distinguishes genuine cache IDs
                    // from the synthetic identities used by pre-INSERT projection.
                    out = out.for_cache();
                    crate::compute::run_candidate(chain, tx, &records, &mut out, scope).await?;
                    out.validate_virtual(tx, &ids)?;
                    out.admit_html(tx)?;
                    let column = out.column(field);
                    if let Some(stage) = tx.candidate_stage.as_mut() {
                        stage.push(StagedCandidate {
                            model: self.spec.id,
                            field,
                            ids: ids.to_vec(),
                            out,
                        });
                    }
                    column
                }
            } else {
                let related = tx.path_inputs(&records, fc, &inputs).await?;
                let out = crate::compute::Outputs::new(
                    env.clone(),
                    self.spec.id,
                    fc.method,
                    &fc.group,
                    ids.len(),
                );
                let out = out.for_cache();
                let mut data = crate::compute::BatchData::new(Arc::clone(&ids), inputs, out)
                    .with_related(related);
                crate::compute::run_sync(chain, 0, &mut data)?;
                data.out.validate_virtual(tx, &ids)?;
                data.out.admit_html(tx)?;
                data.out.take(field)
            };
            if outputs.len() != ids.len() {
                return Err(Error::programming(
                    "Monetary compute projection returned an incomplete column",
                ));
            }
            for (i, position) in chunk.iter().enumerate() {
                result[*position] = match &outputs[i] {
                    Some(cell) => Some(cell.clone()),
                    None if fc.stored => Some(
                        old.as_ref().ok_or_else(|| {
                            Error::programming("Missing monetary compute old values")
                        })?[i]
                            .clone(),
                    ),
                    None if !self.spec.field_by_id(field).readonly => Some(Cell::Null),
                    None => {
                        return Err(Error::programming(format!(
                            "Compute method failed to assign {}({},).{}",
                            self.spec.name,
                            ids[i],
                            self.spec.field_by_id(field).name
                        )));
                    }
                };
            }
        }
        Ok(result)
    }

    // Positional roots for company-bound traversal, including nested paths and
    // final relational targets. Before INSERT these are synthetic identities,
    // not precompute batch records. Bound::of supplies no Lenient prefetch set:
    // refusals retain the root in their diagnostic and never tolerate a row.
    fn starts(&self, tx: &mut Transaction) -> Result<Vec<Option<RecId>>> {
        self.active
            .iter()
            .enumerate()
            .map(|(i, active)| {
                if !active {
                    return Ok(None);
                }
                match self.ids {
                    Some(ids) => Ok(Some(ids[i])),
                    None => tx.next_new_id().map(Some),
                }
            })
            .collect()
    }

    /// Read a currency result with privileged field access while native computation
    /// retains the original environment and its declared compute_sudo policy.
    async fn read_resolved(
        &self,
        tx: &mut Transaction,
        env: &Env,
        field: FieldId,
        given: &[Option<Cell>],
    ) -> Result<Vec<Option<Cell>>> {
        if !self.currency_lookup {
            return self.read(tx, env, field, given).await;
        }
        let ids = self
            .ids
            .ok_or_else(|| Error::programming("Monetary read has no record ids"))?;
        let wanted: Vec<Option<RecId>> = ids
            .iter()
            .zip(given)
            .map(|(id, cell)| cell.is_none().then_some(*id))
            .collect();
        let values = tx
            .currency_cells_by_id(env, (self.spec.id, field), &wanted)
            .await?;
        Ok(wanted
            .into_iter()
            .map(|id| id.and_then(|id| values.get(&id).cloned()))
            .collect())
    }

    async fn read(
        &self,
        tx: &mut Transaction,
        env: &Env,
        field: FieldId,
        given: &[Option<Cell>],
    ) -> Result<Vec<Option<Cell>>> {
        let ids = self
            .ids
            .ok_or_else(|| Error::programming("Monetary read has no record ids"))?;
        let wanted: Vec<Option<RecId>> = ids
            .iter()
            .zip(given)
            .map(|(id, cell)| cell.is_none().then_some(*id))
            .collect();
        let values = tx.cells_by_id(env, (self.spec.id, field), &wanted).await?;
        Ok(wanted
            .into_iter()
            .map(|id| id.and_then(|id| values.get(&id).cloned()))
            .collect())
    }
}

/// Null and zero amounts have no currency prerequisite on any mutation path.
pub(crate) fn needs_currency(cell: &Cell) -> bool {
    !(matches!(cell, Cell::Null)
        || matches!(cell, Cell::Decimal(amount) if *amount == Decimal::ZERO))
}

/// Normalize only the selected monetary columns. A pending precompute currency may wait
/// until its group runs; a consumer that needs the value sets `required`.
/// Returns resolved (row, field) positions, including no-currency preservation, so
/// precompute can retry only unresolved cells and avoid repeated provider calls.
pub(crate) async fn normalize(
    tx: &mut Transaction,
    env: &Env,
    stage: Stage<'_>,
    fields: &[FieldId],
    mixed_assignment: bool,
    required: bool,
) -> Result<Vec<(usize, FieldId)>> {
    if stage.initial
        && stage
            .ids
            .is_none_or(|ids| ids.iter().any(|id| !id.is_new()))
    {
        return Err(Error::programming(
            "Initial monetary staging requires in-memory record ids",
        ));
    }
    let expected_width = stage.fields.len();
    if stage.ids.is_some_and(|ids| ids.len() != stage.rows.len())
        || stage.rows.iter().any(|r| r.len() != expected_width)
    {
        return Err(Error::programming("Invalid monetary staging dimensions"));
    }
    if stage.included.is_some_and(|mask| {
        mask.len() != stage.rows.len() || mask.iter().any(|r| r.len() != expected_width)
    }) {
        return Err(Error::programming("Invalid monetary staging mask"));
    }
    let included = |row: usize, slot: usize| stage.included.is_none_or(|mask| mask[row][slot]);
    // A requested monetary field outside the slot list would be published unrounded.
    let slot_of = |field: FieldId| -> Result<usize> {
        stage.fields.binary_search(&field).map_err(|_| {
            Error::programming(format!(
                "Monetary staging has no slot for {}.{}",
                stage.spec.name,
                stage.spec.field_by_id(field).name
            ))
        })
    };
    // The accounting unit: staged cells of the requested monetary fields, once per record-field.
    let mut cells = 0usize;
    let mut projected: Vec<(FieldId, Vec<usize>)> = Vec::new();
    for (n, &field) in fields.iter().enumerate() {
        if !matches!(stage.spec.field_by_id(field).ty, FieldType::Monetary { .. })
            || fields[..n].contains(&field)
        {
            continue;
        }
        let slot = slot_of(field)?;
        let staged = stage
            .rows
            .iter()
            .enumerate()
            .filter(|(i, row)| included(*i, slot) && row[slot].is_some());
        match stage.kind {
            StageKind::Direct => {
                cells = cells.saturating_add(
                    staged
                        .filter(|(i, _)| !stage.precharged.is_some_and(|m| m[*i][slot]))
                        .count(),
                );
            }
            StageKind::Projection { positions } => projected.push((
                field,
                staged.map(|(i, _)| positions.map_or(i, |p| p[i])).collect(),
            )),
        }
    }
    stage.budget.charge(cells)?;
    for (field, positions) in &projected {
        stage.budget.charge_projected(*field, positions)?;
    }
    let mut completed = Vec::new();
    for &field in fields {
        let fs = stage.spec.field_by_id(field);
        let FieldType::Monetary { currency_field } = &fs.ty else {
            continue;
        };
        let f_idx = slot_of(field)?;
        if !stage
            .rows
            .iter()
            .enumerate()
            .any(|(i, r)| included(i, f_idx) && r[f_idx].is_some())
        {
            continue;
        }
        let currency = currency_field
            .as_deref()
            .and_then(|name| stage.spec.field_id(name))
            .ok_or_else(|| {
                Error::programming(format!(
                    "{}.{} has no resolved monetary currency field",
                    stage.spec.name, fs.name
                ))
            })?;
        let active: Vec<bool> = stage
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| included(i, f_idx) && row[f_idx].as_ref().is_some_and(needs_currency))
            .collect();
        for (i, row) in stage.rows.iter().enumerate() {
            if included(i, f_idx) && row[f_idx].is_some() && !active[i] {
                completed.push((i, field));
            }
        }
        if !active.iter().any(|active| *active) {
            continue;
        }
        let currencies = Inputs {
            producer: env,
            currency_lookup: true,
            initial: stage.initial,
            env,
            spec: stage.spec,
            ids: stage.ids,
            fields: stage.fields,
            rows: stage.rows,
            active: &active,
            included: stage.included,
            budget: stage.budget,
            positions: match stage.kind {
                StageKind::Direct => None,
                StageKind::Projection { positions } => positions,
            },
        }
        .resolve(tx, currency)
        .await?;
        if currencies.len() != stage.rows.len() {
            return Err(Error::programming(
                "Monetary currency resolution returned an incomplete column",
            ));
        }
        // Resolve/validate every active prerequisite before mixed-currency admission.
        let mut resolved = vec![None; currencies.len()];
        let mut unique = None;
        let mut multiple = false;
        for (i, cell) in currencies.iter().enumerate() {
            if !active[i] {
                continue;
            }
            let Some(cell) = cell else {
                if required {
                    if tx
                        .shared
                        .graph
                        .model(stage.spec.id)
                        .compute(currency)
                        .is_some_and(|fc| fc.is_async && !fc.candidate)
                    {
                        return Err(async_currency_refusal(
                            "resolve",
                            stage.spec.name,
                            fs.name,
                            "before create",
                        ));
                    }
                    return Err(Error::programming(format!(
                        "Unresolved monetary currency prerequisite for {}.{}",
                        stage.spec.name, fs.name,
                    )));
                }
                continue;
            };
            let id = match cell {
                Cell::Null => None,
                Cell::Id(_) => Some(crate::relational::id_of(cell).ok_or_else(|| {
                    Error::programming(format!(
                        "Invalid currency id for {}.{}",
                        stage.spec.name, fs.name,
                    ))
                })?),
                _ => {
                    return Err(Error::validation(format!(
                        "Invalid monetary currency value for {}.{}",
                        stage.spec.name, fs.name,
                    )));
                }
            };
            if let Some(id) = id {
                if id.is_new() {
                    return Err(Error::programming(format!(
                        "Cannot normalize {}.{} using an in-memory currency until a later release",
                        stage.spec.name, fs.name,
                    )));
                }
                multiple |= unique.is_some_and(|previous| previous != id);
                unique = Some(id);
            }
            resolved[i] = id;
            completed.push((i, field));
        }
        if mixed_assignment && multiple {
            return Err(Error::validation(format!(
                "Got multiple currencies while assigning values of monetary field {}.{}",
                stage.spec.name, fs.name
            )));
        }
        let mut positions = Vec::new();
        let mut amounts = Vec::new();
        for (i, row) in stage.rows.iter().enumerate() {
            if !active[i] {
                continue;
            }
            let Some(cell) = &row[f_idx] else {
                continue;
            };
            let Some(id) = (if mixed_assignment {
                unique
            } else {
                resolved[i]
            }) else {
                continue;
            };
            match cell {
                Cell::Null => continue,
                Cell::Decimal(amount) => {
                    positions.push(i);
                    amounts.push((id, *amount));
                }
                _ => {
                    return Err(Error::programming(format!(
                        "Invalid staged monetary amount for {}.{}",
                        stage.spec.name, fs.name
                    )));
                }
            }
        }
        if amounts.is_empty() {
            continue;
        }
        let provider = env.registry().model("res.currency").ok_or_else(|| {
            Error::programming(format!(
                "{}.{} requires res.currency._round_monetary",
                stage.spec.name, fs.name
            ))
        })?;
        if !tx.has_method(provider.id, "_round_monetary") {
            return Err(Error::programming(format!(
                "{}.{} requires res.currency._round_monetary",
                stage.spec.name, fs.name
            )));
        }
        // Only currency lookup/provider reads are elevated, never amount publication.
        let records = Records::empty(env.sudo(true), provider.id);
        // At most 256 distinct currencies and recompute_batch rows per native call.
        let batch = tx.config().recompute_batch.clamp(1, 256);
        for (positions, amounts) in positions.chunks(batch).zip(amounts.chunks(batch)) {
            let rounded = records
                .call::<(Vec<(RecId, Decimal)>,), Vec<Decimal>>(
                    tx,
                    "_round_monetary",
                    (amounts.to_vec(),),
                )
                .await?;
            if rounded.len() != positions.len() {
                return Err(Error::programming(format!(
                    "res.currency._round_monetary returned {} amounts, expected {}",
                    rounded.len(),
                    positions.len()
                )));
            }
            for (&position, amount) in positions.iter().zip(rounded) {
                stage.rows[position][f_idx] = Some(Cell::Decimal(amount));
            }
        }
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    fn typed(err: &Error) -> &AdmissionError {
        err.source()
            .and_then(|source| source.downcast_ref::<AdmissionError>())
            .expect("typed admission source")
    }

    #[test]
    fn cell_bound_admits_exactly_the_limit() {
        let budget = StagingBudget::new(1);
        budget.charge(MAX_STAGED_CELLS).unwrap();
        let err = budget.charge(1).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            typed(&err),
            &AdmissionError::MonetaryStagingExceeded {
                records: 1,
                cells: MAX_STAGED_CELLS + 1,
                max_cells: MAX_STAGED_CELLS,
                max_records: MAX_TARGET_RECORDS,
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("monetary staging cells limit (2000000) exceeded"));
        assert!(msg.contains("a later release"));
        assert!(!msg.contains("2000001"));
        // a refused charge records nothing: the remaining budget is still zero
        assert!(budget.check(0).is_ok());
        assert!(budget.check(1).is_err());
    }

    #[test]
    fn record_bound_admits_exactly_the_limit() {
        StagingBudget::new(MAX_TARGET_RECORDS).charge(1).unwrap();
        let err = StagingBudget::new(MAX_TARGET_RECORDS + 1)
            .charge(1)
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        let msg = err.to_string();
        assert!(msg.contains("monetary staging target records limit (200000) exceeded"));
        assert!(msg.contains("a later release"));
        assert!(!msg.contains("200001"));
        // an operation that stages no monetary cell is not a monetary operation
        StagingBudget::new(MAX_TARGET_RECORDS + 1)
            .charge(0)
            .unwrap();
    }

    #[test]
    fn charges_accumulate_across_chunks() {
        let budget = StagingBudget::new(1000);
        for _ in 0..4 {
            budget.charge(MAX_STAGED_CELLS / 4).unwrap();
        }
        assert!(budget.charge(1).is_err());
    }

    #[test]
    fn grid_overflow_is_refused() {
        let budget = StagingBudget::new(1);
        let err = budget.check_grid(usize::MAX, 2).unwrap_err();
        assert_eq!(
            typed(&err),
            &AdmissionError::MonetaryStagingExceeded {
                records: 1,
                cells: usize::MAX,
                max_cells: MAX_STAGED_CELLS,
                max_records: MAX_TARGET_RECORDS,
            }
        );
        budget.charge(1).unwrap();
        assert!(budget.check(usize::MAX).is_err());
    }

    /// The reported limit when both bounds are exceeded is the cell limit.
    #[test]
    fn both_bounds_exceeded_names_the_cell_limit() {
        let err = StagingBudget::new(MAX_TARGET_RECORDS + 1)
            .charge(MAX_STAGED_CELLS + 1)
            .unwrap_err();
        assert!(err.to_string().contains("cells limit (2000000)"));
    }
}
