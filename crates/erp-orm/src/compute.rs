//! Compute methods and constraints as addons declare them (COMP §4.1).
//!
//! `#[compute(...)]` functions in `#[methods(Model)]` blocks contribute [`ComputeImpl`]s to
//! [`COMPUTE_IMPLS`]; `#[constrains(...)]` methods contribute [`ConstraintImpl`]s to
//! [`CONSTRAINT_IMPLS`]. Two flavors of compute exist:
//!
//! - **sync** (the default): `fn(batch: &mut Batch<'_, M>) -> Result<()>`. The engine loads the
//!   declared `depends` and `reads` for the whole batch first; the function reads them from the
//!   batch and assigns the computed fields. It cannot query. Reading a field that is neither
//!   declared nor computed by the same method is a `Programming` error.
//! - **async**: `async fn(this: &RecordSet<M>, tx: &mut Transaction, out: &mut Assign<'_, M>)`,
//!   for computes that search, aggregate or create records. It runs once per batch.
//! - **async candidate** (`#[compute(candidate, ...)]`):
//!   `async fn(this: &RecordSet<M>, tx: &mut ReadTx<'_>, out: &mut Assign<'_, M>)`. An async
//!   compute restricted to ORM reads and SQL `SELECT` through the [`ReadTx`] view, so it may
//!   derive a monetary currency in the same call as the amount. No overrides.
//!
//! Overrides (`#[compute(override, ...)]`) receive the rest of the chain as their last parameter
//! and call it like `super`; their `depends` add to the inherited ones.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use crate::html::input_cell as to_cell;
use erp_fields::convert::{FieldRef, validate_cell};
use erp_fields::{
    Cell, Error, FieldType, FieldValue, IntoFieldValue, RecId, Result, Value, serde_json,
};
use erp_registry::{FieldId, ModelId, ModelSpec};
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::candidate::{ReadScope, ReadTx};
use crate::dispatch::{
    BoxFuture, ImplKind, MAX_WASM_NAME_BYTES, call_method_json, intern, valid_wasm_method,
    valid_wasm_model,
};
use crate::env::Env;
use crate::ir_default::json_to_value;
use crate::records::{Field, Inherits, Model, RecordSet, Records};

/// A sync compute entry (macro output).
pub type SyncComputeFn = fn(&mut BatchData, ComputeSuper) -> Result<()>;

/// An async compute entry (macro output).
pub type AsyncComputeFn = for<'a> fn(
    &'a mut Transaction,
    &'a Records,
    &'a mut Outputs,
    AsyncComputeSuper,
) -> BoxFuture<'a, Result<()>>;

/// An async candidate compute entry (macro output): it sees the transaction only through the
/// read-only [`ReadTx`] view.
pub type CandidateComputeFn =
    for<'a, 't> fn(&'a mut ReadTx<'t>, &'a Records, &'a mut Outputs) -> BoxFuture<'a, Result<()>>;

/// The erased function of a compute implementation.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum ComputeEntry {
    /// Runs over a prefetched batch.
    Sync(SyncComputeFn),
    /// May query; runs once per batch.
    Async(AsyncComputeFn),
    /// An async compute restricted to a read-only view, so that it may join same-call monetary
    /// admission (registry flag `candidate`).
    Candidate(CandidateComputeFn),
}

impl ComputeEntry {
    /// True for async computes, candidates included.
    pub fn is_async(&self) -> bool {
        matches!(self, Self::Async(_) | Self::Candidate(_))
    }

    /// True for async candidate computes (`#[compute(candidate, ...)]`).
    pub fn is_candidate(&self) -> bool {
        matches!(self, Self::Candidate(_))
    }
}

impl std::fmt::Debug for ComputeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sync(_) => "Sync",
            Self::Async(_) => "Async",
            Self::Candidate(_) => "Candidate",
        })
    }
}

/// One compute implementation contributed by an addon.
#[derive(Debug)]
pub struct ComputeImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the `#[methods]` block.
    pub model: &'static str,
    /// Method name (the fields' `compute = "..."`).
    pub method: &'static str,
    /// Define or override.
    pub kind: ImplKind,
    /// `depends` of this implementation (accumulated over the chain).
    pub depends: &'static [&'static str],
    /// Fields read without a trigger.
    pub reads: &'static [&'static str],
    /// Context keys.
    pub depends_context: &'static [&'static str],
    /// The function.
    pub entry: ComputeEntry,
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

impl ComputeImpl {
    pub(crate) fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon)
    }
}

#[derive(Debug)]
pub struct ConstraintImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the `#[methods]` block.
    pub model: &'static str,
    /// Method name; the method itself is in the model's method chain.
    pub method: &'static str,
    /// Field names whose change runs the constraint.
    pub fields: &'static [&'static str],
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

/// Compute implementations linked into the binary.
#[linkme::distributed_slice]
pub static COMPUTE_IMPLS: [ComputeImpl];

/// Constraint declarations linked into the binary.
#[linkme::distributed_slice]
pub static CONSTRAINT_IMPLS: [ConstraintImpl];

/// Whether outputs are staged for cache evaluation or for a persistence path.
/// Negative synthetic pre-INSERT IDs do not confer cache authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Persisted,
    Cache,
}

/// Values assigned by a compute: one optional cell per (computed field, record).
///
/// Cache-only computations may retain virtual many2one references to an origin or an
/// active new record of the comodel in the current transaction. The entire output
/// group is validated before publication; persisted/pre-INSERT destinations refuse
/// virtual targets. An assignment is staged output, not permission to persist an ID.
#[derive(Debug, Clone)]
pub struct Outputs {
    mode: OutputMode,
    model: ModelId,
    env: Env,
    method: &'static str,
    len: usize,
    /// Indexed by field index; `Some` for the fields the method computes.
    values: Vec<Option<Vec<Option<Cell>>>>,
}

impl Outputs {
    pub(crate) fn new(
        env: Env,
        model: ModelId,
        method: &'static str,
        group: &[FieldId],
        len: usize,
    ) -> Self {
        let n = env.registry().model_by_id(model).fields.len();
        let mut values: Vec<Option<Vec<Option<Cell>>>> = (0..n).map(|_| None).collect();
        for f in group {
            values[f.index()] = Some(vec![None; len]);
        }
        Self {
            mode: OutputMode::Persisted,
            model,
            env,
            method,
            len,
            values,
        }
    }

    /// Stage virtual cache references; the transaction validates them before publication.
    pub(crate) fn for_cache(mut self) -> Self {
        self.mode = OutputMode::Cache;
        self
    }

    /// Validate the entire group before any cache mutation or staged-output consumption.
    /// Use current membership: an async compute may allocate a target after Outputs is built.
    pub(crate) fn validate_virtual(&self, tx: &Transaction, ids: &[RecId]) -> Result<()> {
        if ids.len() != self.len {
            return Err(Error::programming(
                "compute output destination count mismatch",
            ));
        }
        let spec = self.spec();
        for field in &spec.fields {
            if !matches!(field.ty, FieldType::Many2one { .. }) {
                continue;
            }
            let Some(column) = &self.values[field.id.index()] else {
                continue;
            };
            for (id, cell) in ids.iter().zip(column) {
                let Some(Cell::Id(raw)) = cell else {
                    continue;
                };
                if *raw >= 0 {
                    continue;
                }
                let cache_destination = self.mode == OutputMode::Cache
                    && id.is_new()
                    && (id.origin().is_some()
                        || tx.engine.new_records.get(id) == Some(&self.model));
                if cache_destination {
                    crate::newrec::new_record_cell(spec, field, Cell::Id(*raw), tx)?;
                } else {
                    validate_cell(
                        FieldRef {
                            model: spec.name,
                            field: field.name,
                            label: &field.string,
                        },
                        &field.ty,
                        Cell::Id(*raw),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn spec(&self) -> &ModelSpec {
        self.env.registry().model_by_id(self.model)
    }

    pub(crate) fn assign(&mut self, i: usize, name: &str, cell: Cell) -> Result<()> {
        let registry = Arc::clone(self.env.registry());
        let spec = registry.model_by_id(self.model);
        let field = spec.field(name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{name}' on '{}'", spec.name))
        })?;
        if i >= self.len {
            return Err(Error::programming(format!(
                "compute method '{}' assigned record #{i} of a batch of {}",
                self.method, self.len
            )));
        }
        let Some(column) = self.values[field.id.index()].as_mut() else {
            return Err(Error::programming(format!(
                "compute method '{}' assigns {}.{}, which it does not compute",
                self.method, spec.name, name
            )));
        };
        // Cache outputs may stage only an encoded virtual many2one here. Identity and
        // destination authority are checked for the whole group before publication.
        let virtual_cell = self.mode == OutputMode::Cache
            && matches!((&field.ty, &cell), (FieldType::Many2one { .. }, Cell::Id(raw))
                if *raw < 0 && RecId::from_raw(*raw).is_some_and(|id| id.origin().is_some() || id.unique_index().is_some()));
        let cell = if virtual_cell || matches!(field.ty, FieldType::Html) {
            cell
        } else {
            validate_cell(
                FieldRef {
                    model: spec.name,
                    field: field.name,
                    label: &field.string,
                },
                &field.ty,
                cell,
            )?
        };
        column[i] = Some(cell);
        Ok(())
    }

    /// Assign the columns a package addon's compute answered: a JSON object, field name to one
    /// value per record of the batch, converted with the field's own type.
    fn assign_columns(&mut self, answer: &str) -> Result<()> {
        let columns: BTreeMap<String, Vec<serde_json::Value>> = serde_json::from_str(answer)
            .map_err(|e| {
                Error::validation(format!(
                    "compute method '{}' of a WebAssembly addon answered something that is not a \
                     list of values per field: {e}",
                    self.method
                ))
            })?;
        let registry = Arc::clone(self.env.registry());
        let spec = registry.model_by_id(self.model);
        for (name, values) in columns {
            let field = spec.field(&name).ok_or_else(|| {
                Error::programming(format!(
                    "compute method '{}' answered the unknown field '{name}' of '{}'",
                    self.method, spec.name
                ))
            })?;
            if values.len() != self.len {
                return Err(Error::validation(format!(
                    "compute method '{}' answered {} values for {}.{name}, expected {}",
                    self.method,
                    values.len(),
                    spec.name,
                    self.len
                )));
            }
            for (i, value) in values.iter().enumerate() {
                let cell = if value.is_null() {
                    Cell::Null
                } else if self.mode == OutputMode::Cache
                    && matches!(field.ty, FieldType::Many2one { .. })
                    && value.as_i64().is_some_and(|raw| raw < 0)
                {
                    // assign checks the encoding; validate_virtual checks current authority.
                    Cell::Id(
                        value
                            .as_i64()
                            .ok_or_else(|| Error::validation("invalid virtual compute id"))?,
                    )
                } else {
                    to_cell(
                        FieldRef {
                            model: spec.name,
                            field: field.name,
                            label: &field.string,
                        },
                        &field.ty,
                        &json_to_value(value),
                    )?
                };
                self.assign(i, &name, cell)?;
            }
        }
        Ok(())
    }

    fn assigned(&self, fid: FieldId, i: usize) -> Option<&Cell> {
        self.values.get(fid.index())?.as_ref()?.get(i)?.as_ref()
    }

    /// The assigned column of `fid`, without consuming it.
    pub(crate) fn column(&self, fid: FieldId) -> Vec<Option<Cell>> {
        self.values
            .get(fid.index())
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn admit_html(&mut self, tx: &mut Transaction) -> Result<()> {
        let registry = Arc::clone(self.env.registry());
        let spec = registry.model_by_id(self.model);
        for field in &spec.fields {
            if !matches!(field.ty, FieldType::Html) {
                continue;
            }
            if let Some(column) = &mut self.values[field.id.index()] {
                for value in column.iter_mut().flatten() {
                    *value = tx.admit_html_cell(spec, field, value.clone())?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn take(&mut self, fid: FieldId) -> Vec<Option<Cell>> {
        self.values
            .get_mut(fid.index())
            .and_then(Option::take)
            .unwrap_or_default()
    }
}

/// The input of a sync compute: the batch's ids and the prefetched values it may read, plus the
/// values it assigns.
#[derive(Debug)]
pub struct BatchData {
    ids: Arc<[RecId]>,
    /// Indexed by field index; `Some` for readable fields.
    inputs: Vec<Option<Vec<Cell>>>,
    /// Values of other records reached through the compute's paths (S4b).
    related: Vec<RelColumn>,
    pub(crate) out: Outputs,
}

/// The values of one field for the records a compute reaches through its paths
/// (`line_ids.price_total`: `price_total` of the lines of the batch's records).
#[derive(Debug)]
pub(crate) struct RelColumn {
    pub(crate) model: ModelId,
    pub(crate) field: FieldId,
    pub(crate) values: FxHashMap<RecId, Cell>,
}

impl RelColumn {
    pub(crate) fn new(model: ModelId, field: FieldId) -> Self {
        Self {
            model,
            field,
            values: FxHashMap::default(),
        }
    }
}

impl BatchData {
    pub(crate) fn new(ids: Arc<[RecId]>, inputs: Vec<Option<Vec<Cell>>>, out: Outputs) -> Self {
        Self {
            ids,
            inputs,
            related: Vec::new(),
            out,
        }
    }

    /// With the values read through the compute's paths.
    pub(crate) fn with_related(mut self, related: Vec<RelColumn>) -> Self {
        self.related = related;
        self
    }

    /// The value of `field` (of a model named or inherited as `model`) for record `id`, read
    /// through one of the compute's paths.
    fn read_related(&self, model: &str, field: &str, id: RecId) -> Result<Cell> {
        let registry = self.out.env.registry();
        let mut declared = false;
        for column in &self.related {
            let spec = registry.model_by_id(column.model);
            if spec.field_by_id(column.field).name != field || !spec.inherits(model) {
                continue;
            }
            declared = true;
            if let Some(cell) = column.values.get(&id) {
                return Ok(cell.clone());
            }
        }
        if declared {
            return Err(Error::programming(format!(
                "compute method '{}' reads {model}.{field} of record {id}, which its paths do not \
                 reach from the batch's records",
                self.out.method
            )));
        }
        Err(Error::programming(format!(
            "compute method '{}' reads {model}.{field}, which is in none of its depends or reads \
             paths: declare the path (e.g. \"line_ids.{field}\"), or use an async \
             compute",
            self.out.method
        )))
    }

    fn read(&self, i: usize, name: &str) -> Result<Cell> {
        let spec = self.out.spec();
        let field = spec.field(name).ok_or_else(|| {
            Error::programming(format!("Invalid field '{name}' on '{}'", spec.name))
        })?;
        if field.name == "id" {
            return self
                .ids
                .get(i)
                .map(|id| Cell::Id(id.get()))
                .ok_or_else(|| self.out_of_range(i));
        }
        if let Some(cell) = self.out.assigned(field.id, i) {
            return Ok(cell.clone());
        }
        match self.inputs.get(field.id.index()).and_then(Option::as_ref) {
            Some(column) => column.get(i).cloned().ok_or_else(|| self.out_of_range(i)),
            None => Err(Error::programming(format!(
                "compute method '{}' reads {}.{}, which is neither in its depends nor in its \
                 reads: declare it, or use an async compute",
                self.out.method, spec.name, name
            ))),
        }
    }

    fn out_of_range(&self, i: usize) -> Error {
        Error::programming(format!(
            "record #{i} is out of a batch of {}",
            self.ids.len()
        ))
    }
}

/// A sync compute's view of its batch, typed by the model of the `#[methods]` block.
pub struct Batch<'a, M> {
    data: &'a mut BatchData,
    _m: PhantomData<fn() -> M>,
}

impl<'a, M: Model> Batch<'a, M> {
    /// Macro output: checked that the batch's model is `M` or inherits it.
    #[doc(hidden)]
    pub fn wrap(data: &'a mut BatchData) -> Result<Self> {
        let spec = data.out.spec();
        if !spec.inherits(M::MODEL_NAME) {
            return Err(Error::programming(format!(
                "a batch of '{}' is not a batch of '{}' or of a model inheriting it",
                spec.name,
                M::MODEL_NAME
            )));
        }
        Ok(Self {
            data,
            _m: PhantomData,
        })
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.data.ids.len()
    }

    /// No records.
    pub fn is_empty(&self) -> bool {
        self.data.ids.is_empty()
    }

    /// Record indexes `0..len`.
    pub fn indices(&self) -> std::ops::Range<usize> {
        0..self.len()
    }

    /// The record ids (new records have negative ids).
    pub fn ids(&self) -> &[RecId] {
        &self.data.ids
    }

    /// The environment the compute runs in (superuser when the field is `compute_sudo`).
    pub fn env(&self) -> &Env {
        &self.data.out.env
    }

    /// The records of the batch.
    pub fn records(&self) -> RecordSet<M> {
        let ids = Arc::clone(&self.data.ids);
        RecordSet::unchecked(Records::from_parts(
            self.data.out.env.clone(),
            self.data.out.model,
            Arc::clone(&ids),
            ids,
        ))
    }

    /// The value of a declared field for record `i` (or the value this compute assigned).
    pub fn get<P: Model, T: FieldValue>(&self, i: usize, field: Field<P, T>) -> Result<T>
    where
        M: Inherits<P>,
    {
        T::from_cell(&self.data.read(i, field.name())?)
    }

    /// The values of a declared field for every record, in order.
    pub fn values<P: Model, T: FieldValue>(&self, field: Field<P, T>) -> Result<Vec<T>>
    where
        M: Inherits<P>,
    {
        self.indices()
            .map(|i| T::from_cell(&self.data.read(i, field.name())?))
            .collect()
    }

    /// The value of a field of another record reached through one of the compute's paths:
    /// with `depends = ["line_ids.price"]`, `batch.value_of(Line::PRICE, line)` for every line of
    /// `batch.get(i, Order::LINE_IDS)`. `C` is the comodel (or a model it inherits). Reading a
    /// field no declared path reaches is an error, as for the record's own fields.
    pub fn value_of<C: Model, T: FieldValue>(&self, field: Field<C, T>, id: RecId) -> Result<T> {
        T::from_cell(&self.data.read_related(C::MODEL_NAME, field.name(), id)?)
    }

    /// [`Batch::value_of`] for several records, in order.
    pub fn values_of<C: Model, T: FieldValue>(
        &self,
        field: Field<C, T>,
        ids: &[RecId],
    ) -> Result<Vec<T>> {
        ids.iter().map(|id| self.value_of(field, *id)).collect()
    }

    /// Stage a computed field of record `i`. Ordinary values are validated like writes;
    /// cache-only virtual many2one identities are checked at whole-group publication
    /// against the current transaction and destination (see [`Outputs`]).
    pub fn set<P: Model, T: FieldValue, V: IntoFieldValue<T>>(
        &mut self,
        i: usize,
        field: Field<P, T>,
        value: V,
    ) -> Result<()>
    where
        M: Inherits<P>,
    {
        let cell = value.into_field_value().into_cell();
        self.data.out.assign(i, field.name(), cell)
    }

    /// Assign the same value to every record (`self.field = value`).
    pub fn set_all<P: Model, T: FieldValue + Clone, V: IntoFieldValue<T>>(
        &mut self,
        field: Field<P, T>,
        value: V,
    ) -> Result<()>
    where
        M: Inherits<P>,
    {
        let cell = value.into_field_value().into_cell();
        for i in self.indices() {
            self.data.out.assign(i, field.name(), cell.clone())?;
        }
        Ok(())
    }
}

/// Where an async compute assigns its values, typed by the model of the `#[methods]` block.
/// Record `i` is the `i`-th record of the compute's recordset.
pub struct Assign<'a, M> {
    data: &'a mut Outputs,
    _m: PhantomData<fn() -> M>,
}

impl<'a, M: Model> Assign<'a, M> {
    /// Macro output.
    #[doc(hidden)]
    pub fn wrap(data: &'a mut Outputs) -> Self {
        Self {
            data,
            _m: PhantomData,
        }
    }

    /// Stage a computed field of record `i` (see [`Outputs`] for virtual relation
    /// validation at whole-group publication).
    pub fn set<P: Model, T: FieldValue, V: IntoFieldValue<T>>(
        &mut self,
        i: usize,
        field: Field<P, T>,
        value: V,
    ) -> Result<()>
    where
        M: Inherits<P>,
    {
        let cell = value.into_field_value().into_cell();
        self.data.assign(i, field.name(), cell)
    }

    /// Assign the same value to every record.
    pub fn set_all<P: Model, T: FieldValue + Clone, V: IntoFieldValue<T>>(
        &mut self,
        field: Field<P, T>,
        value: V,
    ) -> Result<()>
    where
        M: Inherits<P>,
    {
        let cell = value.into_field_value().into_cell();
        for i in 0..self.data.len {
            self.data.assign(i, field.name(), cell.clone())?;
        }
        Ok(())
    }
}

/// The rest of a sync compute chain (`super()`).
pub struct ComputeSuper {
    pub(crate) chain: Arc<[&'static ComputeImpl]>,
    pub(crate) next: usize,
}

impl ComputeSuper {
    /// Run the next implementation on the same batch.
    pub fn call<M>(self, batch: &mut Batch<'_, M>) -> Result<()> {
        run_sync(&self.chain, self.next, batch.data)
    }
}

pub(crate) fn run_sync(
    chain: &Arc<[&'static ComputeImpl]>,
    index: usize,
    data: &mut BatchData,
) -> Result<()> {
    let Some(imp) = chain.get(index) else {
        return Err(Error::programming(
            "super() of a compute method called on the last implementation",
        ));
    };
    let ComputeEntry::Sync(f) = imp.entry else {
        return Err(Error::programming(format!(
            "compute method '{}' at {} is async in a sync chain",
            imp.method,
            imp.location()
        )));
    };
    f(
        data,
        ComputeSuper {
            chain: Arc::clone(chain),
            next: index + 1,
        },
    )
}

/// The rest of an async compute chain (`super()`).
pub struct AsyncComputeSuper {
    pub(crate) chain: Arc<[&'static ComputeImpl]>,
    pub(crate) next: usize,
}

impl AsyncComputeSuper {
    /// Run the next implementation on the same records and outputs.
    pub fn call<'a, M>(
        self,
        tx: &'a mut Transaction,
        records: &'a Records,
        out: &'a mut Assign<'_, M>,
    ) -> BoxFuture<'a, Result<()>> {
        run_async(self.chain, self.next, tx, records, out.data)
    }
}

pub(crate) fn run_async<'a>(
    chain: Arc<[&'static ComputeImpl]>,
    index: usize,
    tx: &'a mut Transaction,
    records: &'a Records,
    out: &'a mut Outputs,
) -> BoxFuture<'a, Result<()>> {
    let Some(imp) = chain.get(index).copied() else {
        return Box::pin(async {
            Err(Error::programming(
                "super() of a compute method called on the last implementation",
            ))
        });
    };
    let ComputeEntry::Async(f) = imp.entry else {
        let message = format!(
            "compute method '{}' at {} is not a plain async compute (a candidate compute runs \
             through its read-only view, a sync one in a batch)",
            imp.method,
            imp.location()
        );
        return Box::pin(async move { Err(Error::programming(message)) });
    };
    f(
        tx,
        records,
        out,
        AsyncComputeSuper {
            chain,
            next: index + 1,
        },
    )
}

/// Run a candidate compute (the only implementation of its chain: the registry refuses
/// overrides) over `records` with the given view of the transaction.
pub(crate) async fn run_candidate(
    chain: &Arc<[&'static ComputeImpl]>,
    tx: &mut Transaction,
    records: &Records,
    out: &mut Outputs,
    scope: ReadScope,
) -> Result<()> {
    let Some(imp) = chain.first().copied() else {
        return Err(Error::programming(
            "a candidate compute has no implementation",
        ));
    };
    let ComputeEntry::Candidate(f) = imp.entry else {
        return Err(Error::programming(format!(
            "compute method '{}' at {} is not a candidate compute",
            imp.method,
            imp.location()
        )));
    };
    let mut view = ReadTx::new(tx, scope);
    f(&mut view, records, out).await
}

// ---- computes and constraints declared by a package ------------------------

/// The most compute and constraint implementations a process interns for loaded packages.
const MAX_WASM_CHECKS: usize = 16_384;
/// Most field names one compute or constraint of a package may list.
const MAX_WASM_FIELD_LIST: usize = 256;

/// A scalar compute that a package addon's component runs. The component computes from the
/// values of its `depends` and `reads` fields, which the host reads and hands over as data, so
/// the compute also works on new records, and never queries (SPEC-WASM).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WasmComputeSpec {
    /// The package addon.
    pub addon: String,
    /// `_name` of the model of its `#[methods]` block.
    pub model: String,
    /// The compute method (the field's `compute`); it is also the name of the component's method.
    pub method: String,
    /// Plain field names the result depends on (no path through a relation yet).
    pub depends: Vec<String>,
    /// Plain field names read without a trigger.
    pub reads: Vec<String>,
    /// Context keys the result depends on.
    pub depends_context: Vec<String>,
}

/// A constraint method that a package addon's component runs after a write or create touched one
/// of `fields`. The method itself is a method of the component named `method`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WasmConstraintSpec {
    /// The package addon.
    pub addon: String,
    /// `_name` of the model of its `#[methods]` block.
    pub model: String,
    /// The constraint method.
    pub method: String,
    /// The fields whose change runs it.
    pub fields: Vec<String>,
}

#[derive(Default)]
struct WasmChecks {
    computes: HashMap<WasmComputeSpec, &'static ComputeImpl>,
    constraints: HashMap<WasmConstraintSpec, &'static ConstraintImpl>,
}

static WASM_CHECKS: Mutex<Option<WasmChecks>> = Mutex::new(None);

fn check_field_names(what: &str, owner: &str, names: &[String]) -> Result<()> {
    if names.len() > MAX_WASM_FIELD_LIST {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!("{owner}: {what} lists more than {MAX_WASM_FIELD_LIST} names"),
        ));
    }
    for name in names {
        if !valid_wasm_method(name) {
            return Err(Error::programming(format!(
                "{owner}: {what} names '{}', which is not a plain field name (ASCII letters, \
                 digits and '_'; a path through a relation is not available in a package yet)",
                name.chars().take(64).collect::<String>()
            )));
        }
    }
    Ok(())
}

fn check_owner(addon: &str, model: &str, method: &str) -> Result<()> {
    if !valid_wasm_model(model) || !valid_wasm_method(method) || !valid_wasm_method(addon) {
        return Err(Error::programming(format!(
            "invalid name '{}.{}' of addon '{}' in a WebAssembly compute or constraint: a model \
             is lower-case ASCII letters, digits, '_' and '.', the others ASCII letters, digits \
             and '_', each at most {MAX_WASM_NAME_BYTES} bytes",
            model.chars().take(64).collect::<String>(),
            method.chars().take(64).collect::<String>(),
            addon.chars().take(64).collect::<String>(),
        )));
    }
    Ok(())
}

fn intern_list(names: &[String]) -> Result<&'static [&'static str]> {
    let interned = names
        .iter()
        .map(|n| intern(n))
        .collect::<Result<Vec<&'static str>>>()?;
    Ok(Box::leak(interned.into_boxed_slice()))
}

/// The [`ComputeImpl`] of a package addon's compute, interned: the same spec returns the same
/// implementation on every registry build, so a rebuild leaks nothing. Names are validated before
/// anything is interned, and the process holds at most 16,384 such implementations.
///
/// # Errors
/// `Programming` for a name that is not a plain identifier, `ResourceExhausted` past the caps.
pub fn wasm_compute_impl(spec: &WasmComputeSpec) -> Result<&'static ComputeImpl> {
    check_owner(&spec.addon, &spec.model, &spec.method)?;
    let owner = format!("compute '{}' of '{}'", spec.method, spec.model);
    check_field_names("depends", &owner, &spec.depends)?;
    check_field_names("reads", &owner, &spec.reads)?;
    check_field_names("depends_context", &owner, &spec.depends_context)?;
    let mut guard = WASM_CHECKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let checks = guard.get_or_insert_with(WasmChecks::default);
    if let Some(known) = checks.computes.get(spec) {
        return Ok(known);
    }
    if checks.computes.len() + checks.constraints.len() >= MAX_WASM_CHECKS {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!(
                "more than {MAX_WASM_CHECKS} computes and constraints of packages in this \
                 process; refusing {owner}"
            ),
        ));
    }
    let imp: &'static ComputeImpl = Box::leak(Box::new(ComputeImpl {
        addon: intern(&spec.addon)?,
        model: intern(&spec.model)?,
        method: intern(&spec.method)?,
        kind: ImplKind::Define,
        depends: intern_list(&spec.depends)?,
        reads: intern_list(&spec.reads)?,
        depends_context: intern_list(&spec.depends_context)?,
        entry: ComputeEntry::Async(wasm_compute_entry),
        file: "<wasm>",
        line: 0,
    }));
    checks.computes.insert(spec.clone(), imp);
    Ok(imp)
}

/// The [`ConstraintImpl`] of a package addon's constraint, interned like [`wasm_compute_impl`].
///
/// # Errors
/// As [`wasm_compute_impl`].
pub fn wasm_constraint_impl(spec: &WasmConstraintSpec) -> Result<&'static ConstraintImpl> {
    check_owner(&spec.addon, &spec.model, &spec.method)?;
    let owner = format!("constraint '{}' of '{}'", spec.method, spec.model);
    check_field_names("fields", &owner, &spec.fields)?;
    let mut guard = WASM_CHECKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let checks = guard.get_or_insert_with(WasmChecks::default);
    if let Some(known) = checks.constraints.get(spec) {
        return Ok(known);
    }
    if checks.computes.len() + checks.constraints.len() >= MAX_WASM_CHECKS {
        return Err(Error::new(
            erp_fields::ErrorKind::ResourceExhausted,
            format!(
                "more than {MAX_WASM_CHECKS} computes and constraints of packages in this \
                 process; refusing {owner}"
            ),
        ));
    }
    let imp: &'static ConstraintImpl = Box::leak(Box::new(ConstraintImpl {
        addon: intern(&spec.addon)?,
        model: intern(&spec.model)?,
        method: intern(&spec.method)?,
        fields: intern_list(&spec.fields)?,
        file: "<wasm>",
        line: 0,
    }));
    checks.constraints.insert(spec.clone(), imp);
    Ok(imp)
}

/// The cell of an input as the component reads it: `null` for an empty value, otherwise the
/// field's JSON form.
fn input_json(cell: &Cell) -> Result<serde_json::Value> {
    match cell {
        Cell::Null => Ok(serde_json::Value::Null),
        other => crate::wasm_abi1_value_to_json(&Value::from_cell(other)),
    }
}

/// The entry of every compute that runs in a component: hand the component the inputs of the
/// batch as data, call the method of that name through the chain (so the host's identity, limits
/// and instance pool apply), and assign what it answers.
fn wasm_compute_entry<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    out: &'a mut Outputs,
    sup: AsyncComputeSuper,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let Some(imp) = sup
            .next
            .checked_sub(1)
            .and_then(|i| sup.chain.get(i).copied())
        else {
            return Err(Error::programming(
                "a WebAssembly compute was started without its implementation",
            ));
        };
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let inputs: Vec<String> = imp
            .depends
            .iter()
            .chain(imp.reads.iter())
            .map(|name| (*name).to_owned())
            .collect();
        let native_output = spec.fields.iter().any(|field| {
            out.values
                .get(field.id.index())
                .is_some_and(Option::is_some)
                && crate::wasm_abi1::native_type(&field.ty).is_some()
        });
        let native_input = inputs.iter().any(|name| {
            spec.field(name)
                .is_some_and(|field| crate::wasm_abi1::native_type(&field.ty).is_some())
        });
        if native_output || native_input {
            records.check_access(tx, crate::Operation::Read).await?;
            tx.check_field_names(
                records.env(),
                records.model_id(),
                inputs.iter().map(String::as_str),
                crate::FieldOp::Read,
            )?;
            tx.check_fields(
                records.env(),
                records.model_id(),
                spec.fields
                    .iter()
                    .filter(|field| {
                        out.values
                            .get(field.id.index())
                            .is_some_and(Option::is_some)
                    })
                    .map(|field| field.id),
                crate::FieldOp::Read,
            )?;
            if native_output {
                return Err(crate::wasm_abi1::native_error("native compute outputs are not supported across frozen WASM ABI1; requires a future ABI major with typed native fields".to_owned()));
            }
            crate::wasm_abi1_check_fields(tx, records, crate::Operation::Read, &inputs).await?;
        }
        let mut columns = serde_json::Map::new();
        for name in imp.depends.iter().chain(imp.reads.iter()) {
            if columns.contains_key(*name) {
                continue;
            }
            spec.field(name).ok_or_else(|| {
                Error::programming(format!(
                    "compute method '{}' reads the unknown field '{name}' of '{}'",
                    imp.method, spec.name
                ))
            })?;
            // read as the invoking environment reads them: access rights, record rules and field
            // groups apply (security review r1 H2)
            let cells = records.cells(tx, name).await?;
            let values = cells.iter().map(input_json).collect::<Result<Vec<_>>>()?;
            columns.insert((*name).to_owned(), serde_json::Value::Array(values));
        }
        let ids: Vec<i64> = records.rec_ids().iter().map(|id| id.get()).collect();
        let batch = serde_json::json!({ "ids": ids, "columns": columns });
        let args = serde_json::to_string(&[batch])
            .map_err(|e| Error::programming(format!("compute inputs are not serialisable: {e}")))?;
        // the component gets the inputs as data, so it is invoked on no records: new records
        // (negative ids) never cross as record ids
        let none = records.browse_ids(Vec::new());
        let answer = call_method_json(tx, &none, imp.method, &args).await?;
        out.assign_columns(&answer)
    })
}

#[cfg(test)]
mod virtual_output_tests {
    use super::*;
    use crate::context::Context;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "virtual_output_test",
        depends: &["base"],
        exact_only: false,
    };
    static MODEL: ModelDecl = ModelDecl::new("virtual_output_test", "virtual_output_test.node")
        .fields(const { &[FieldDecl::many2one("target_id", "virtual_output_test.node")] });

    fn outputs() -> Outputs {
        let registry = RegistryBuilder::new([&ADDON])
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let model = registry.model("virtual_output_test.node").unwrap();
        let (mid, fid) = (model.id, model.field("target_id").unwrap().id);
        let env = Env::new(
            Arc::new(registry),
            crate::principal::Principal::for_uid(crate::UserId::new(2).unwrap()),
            Context::new(),
        );
        Outputs::new(env, mid, "_fixture", &[fid], 1)
    }

    #[test]
    fn package_json_preserves_virtual_encoding_only_for_cache_staging() {
        let id = RecId::new_unique(1).unwrap();
        let answer = format!("{{\"target_id\":[{}]}}", id.get());
        let err = outputs().assign_columns(&answer).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "Invalid value for virtual_output_test.node.target_id: {}",
                id.get()
            )
        );
        let mut cache = outputs().for_cache();
        cache.assign_columns(&answer).unwrap();
        let fid = cache.spec().field("target_id").unwrap().id;
        assert_eq!(cache.column(fid), vec![Some(Cell::Id(id.get()))]);
        // Successful staging does not publish to any transaction; whole-group admission is
        // exercised by compute_virtual_many2one before cache/persistence publication.
        let err = cache.assign_columns("{\"target_id\":[-1.5]}").unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "Invalid value for virtual_output_test.node.target_id: -1.5"
        );
        let err = cache
            .assign_columns("{\"target_id\":[-9223372036854775808]}")
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "Invalid value for virtual_output_test.node.target_id: -9223372036854775808"
        );
    }
}
