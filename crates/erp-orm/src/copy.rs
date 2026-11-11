//! `copy` and `copy_data` (CRUD §4.7).
//!
//! `copy` and `copy_data` are model methods with override chains ([`BUILTIN_IMPLS`]), like `create`,
//! `write`, and `unlink`.

use std::any::Any;
use std::collections::BTreeSet;
use std::sync::Arc;

use erp_fields::{Cell, Error, RecId, Result, Value, ValueMap};
use erp_registry::ModelId;
use rustc_hash::FxHashSet;

use crate::Transaction;
use crate::access::Operation;
use crate::commands::Command;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::{Model, RecordSet, Records};
use crate::vals::RawVals;

/// Maximum recursion depth for nested one2many copies (standards §4).
pub const MAX_COPY_DEPTH: usize = 16;

/// Maximum total records duplicated in a single copy operation (standards §4).
pub const MAX_COPY_RECORDS: usize = 10_000;

/// State for tracking recursion depth, total record cap, and circular relationship guards.
#[derive(Debug)]
pub(crate) struct CopyTracker {
    /// Deterministic set of visited `(model_id, database_id_or_origin)` pairs.
    pub(crate) seen: BTreeSet<(ModelId, i64)>,
    /// Current recursion depth.
    pub(crate) depth: usize,
    /// Total records visited/copied so far.
    pub(crate) total_records: usize,
    max_depth: usize,
    max_records: usize,
}

impl CopyTracker {
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_COPY_DEPTH, MAX_COPY_RECORDS)
    }

    pub(crate) fn with_limits(max_depth: usize, max_records: usize) -> Self {
        Self {
            seen: BTreeSet::new(),
            depth: 0,
            total_records: 0,
            max_depth,
            max_records,
        }
    }

    /// Enter a new nested recursion level, enforcing [`MAX_COPY_DEPTH`].
    pub(crate) fn enter_level(&mut self) -> Result<()> {
        if self.depth >= self.max_depth {
            return Err(Error::validation(format!(
                "copy: nested deeper than {} levels",
                self.max_depth
            )));
        }
        self.depth += 1;
        Ok(())
    }

    /// Exit the current recursion level.
    pub(crate) fn exit_level(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Check if `(model, id)` was already seen.
    ///
    /// If not seen, records it in the set, increments `total_records` (enforcing
    /// [`MAX_COPY_RECORDS`]), and returns `Ok(false)`. If already seen, returns `Ok(true)`.
    pub(crate) fn record_seen(&mut self, model: ModelId, id: i64) -> Result<bool> {
        if self.seen.contains(&(model, id)) {
            return Ok(true);
        }
        if self.total_records >= self.max_records {
            return Err(Error::validation(format!(
                "copy: exceeded limit of {} records",
                self.max_records
            )));
        }
        self.seen.insert((model, id));
        self.total_records += 1;
        Ok(false)
    }
}

impl Records {
    /// Duplicate records, applying explicit defaults before creating each copy.
    /// Generated outputs are omitted; explicit generated defaults (NULL included) return
    /// `Validation` before dispatch.
    ///
    /// Returns access/field validation errors from reading or creating records, a validation
    /// error on traversal limits, or a programming error if duplicate source ids would be skipped.
    /// Addons can override this method and call `Super`.
    ///
    /// ```no_run
    /// # async fn example(records: erp_orm::Records, mut tx: erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let copied = records.copy(&mut tx, None).await?;
    /// assert_eq!(copied.len(), records.len());
    /// # Ok(()) }
    /// ```
    pub async fn copy(&self, tx: &mut Transaction, default: Option<ValueMap>) -> Result<Records> {
        tx.check_records(self)?;
        if let Some(defaults) = &default {
            crate::database_generated::check_write_names(
                tx,
                self.spec(),
                defaults.keys().map(String::as_str),
            )?;
        }
        call_method::<(Option<ValueMap>,), Records>(tx, self, "copy", (default,)).await
    }

    /// Read copyable values, applying defaults and turning one2many children into create commands.
    ///
    /// Already visited records yield `None`; unreadable many2many targets are omitted. Reading
    /// a restricted field returns an access error. Traversal bounds return validation errors.
    /// Translated fields copy their entire translation map (all languages). Generated outputs
    /// are not copied, and explicit generated defaults return `Validation`.
    ///
    /// ```no_run
    /// # async fn example(records: erp_orm::Records, mut tx: erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let data = records.copy_data(&mut tx, None).await?;
    /// assert_eq!(data.len(), records.len());
    /// # Ok(()) }
    /// ```
    pub async fn copy_data(
        &self,
        tx: &mut Transaction,
        default: Option<&ValueMap>,
    ) -> Result<Vec<Option<ValueMap>>> {
        tx.check_records(self)?;
        if let Some(defaults) = default {
            crate::database_generated::check_write_names(
                tx,
                self.spec(),
                defaults.keys().map(String::as_str),
            )?;
        }
        let default = default.cloned();
        call_method::<(Option<ValueMap>,), Vec<Option<ValueMap>>>(tx, self, "copy_data", (default,))
            .await
    }
}

impl<M: Model> RecordSet<M> {
    /// Duplicate recordset updating it with default values (see [`Records::copy`]).
    pub async fn copy(&self, tx: &mut Transaction, default: Option<ValueMap>) -> Result<Self> {
        let copied = self.as_records().copy(tx, default).await?;
        Ok(Self::unchecked(copied))
    }

    /// Copy given records' data with all field values (see [`Records::copy_data`]).
    pub async fn copy_data(
        &self,
        tx: &mut Transaction,
        default: Option<&ValueMap>,
    ) -> Result<Vec<Option<ValueMap>>> {
        self.as_records().copy_data(tx, default).await
    }
}

/// The `base` implementation of `copy`.
async fn base_copy(
    tx: &mut Transaction,
    records: &Records,
    default: Option<ValueMap>,
) -> Result<Records> {
    if records.is_empty() {
        return Ok(records.clone());
    }
    let vals_list = records
        .with_context([("active_test", Value::Bool(false))])
        .copy_data(tx, default.as_ref())
        .await?;
    let rec_list: Vec<Records> = records.iter().collect();
    let mut raw_vals: Vec<RawVals> = Vec::with_capacity(vals_list.len());
    for (i, opt) in vals_list.into_iter().enumerate() {
        let Some(map) = opt else {
            let rec = rec_list.get(i).ok_or_else(|| {
                Error::programming(format!("copy: record index {i} out of bounds"))
            })?;
            let model_name = records.model_name();
            let id = rec
                .rec_ids()
                .first()
                .ok_or_else(|| Error::programming("empty record in rec_list"))?
                .get();
            return Err(Error::programming(format!(
                "copy: record {model_name},{id} already copied in this chain"
            )));
        };
        raw_vals.push(records.vals_from_values(map.iter())?);
    }
    records.create(tx, raw_vals).await
}

fn extract_ids(cells: &[Cell], field_name: &str, model_name: &str) -> Result<Arc<[RecId]>> {
    match cells.first() {
        Some(Cell::Ids(ids)) => Ok(ids.clone()),
        Some(Cell::Null) => Ok(Arc::from([])),
        Some(other) => Err(Error::programming(format!(
            "field '{field_name}' on '{model_name}': expected Cell::Ids or Cell::Null, got {other:?}"
        ))),
        None => Err(Error::programming(format!(
            "field '{field_name}' on '{model_name}': missing cell value"
        ))),
    }
}

fn extract_value(cells: &[Cell], field_name: &str, model_name: &str) -> Result<Value> {
    match cells.first() {
        Some(c) => Ok(Value::from_cell(c)),
        None => Err(Error::programming(format!(
            "field '{field_name}' on '{model_name}': missing cell value"
        ))),
    }
}

/// The `base` implementation of `copy_data`.
async fn base_copy_data(
    tx: &mut Transaction,
    records: &Records,
    default: Option<&ValueMap>,
) -> Result<Vec<Option<ValueMap>>> {
    tx.check_records(records)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    if !records.env().su() {
        tx.prepare_access(records.env()).await?;
    }
    // Verify records exist and caller has read access (L3)
    records.check_access(tx, Operation::Read).await?;

    let is_root = tx.copy_tracker.is_none();
    if is_root {
        let tracker = CopyTracker::new();
        #[cfg(feature = "test-faults")]
        let tracker = match tx.copy_limits_for_tests {
            Some((depth, records)) => CopyTracker::with_limits(depth, records),
            None => tracker,
        };
        tx.copy_tracker = Some(tracker);
    }

    // The guard also clears the root tracker when a caller drops this future.
    let scope = CopyScope { tx, is_root };
    run_copy_data(scope.tx, records, default).await
}

struct CopyScope<'a> {
    tx: &'a mut Transaction,
    is_root: bool,
}

impl Drop for CopyScope<'_> {
    fn drop(&mut self) {
        if self.is_root {
            self.tx.copy_tracker = None;
        }
    }
}

impl Transaction {
    /// Lower copy traversal bounds in tests; production bounds cannot be raised.
    ///
    /// Returns a validation error for zero or a bound above the production limit.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_copy_limits_for_tests(&mut self, depth: usize, records: usize) -> Result<()> {
        if depth == 0 || depth > MAX_COPY_DEPTH || records == 0 || records > MAX_COPY_RECORDS {
            return Err(Error::validation(
                "test copy limits must be positive and no greater than production bounds",
            ));
        }
        self.copy_limits_for_tests = Some((depth, records));
        Ok(())
    }
}

async fn run_copy_data(
    tx: &mut Transaction,
    records: &Records,
    default: Option<&ValueMap>,
) -> Result<Vec<Option<ValueMap>>> {
    let registry = Arc::clone(records.env().registry());
    let spec = registry.model_by_id(records.model_id());

    // 1. Blacklist
    let mut blacklist = FxHashSet::default();
    blacklist.insert("id");
    blacklist.insert("create_uid");
    blacklist.insert("create_date");
    blacklist.insert("write_uid");
    blacklist.insert("write_date");
    blacklist.insert("parent_path");

    let mut blacklist_stack = vec![spec];
    while let Some(m) = blacklist_stack.pop() {
        for &(parent_id, delegate_id) in &m.inherits {
            let parent_field_name = m.field_by_id(delegate_id).name;
            blacklist.insert(parent_field_name);
            if default.is_some_and(|d| d.contains_key(parent_field_name)) {
                let parent_spec = registry.model_by_id(parent_id);
                for f in &parent_spec.fields {
                    if spec
                        .field(f.name)
                        .is_none_or(|local| local.inherited.is_some())
                    {
                        blacklist.insert(f.name);
                    }
                }
            } else {
                blacklist_stack.push(registry.model_by_id(parent_id));
            }
        }
    }

    // 2. Fields to copy
    let fields_to_copy: Vec<&erp_registry::FieldSpec> = spec
        .fields
        .iter()
        .filter(|f| {
            f.copy && !blacklist.contains(f.name) && default.is_none_or(|d| !d.contains_key(f.name))
        })
        .collect();

    let rec_list: Vec<Records> = records.iter().collect();
    let n = rec_list.len();
    let mut vals_list: Vec<Option<ValueMap>> = Vec::with_capacity(n);

    // 3. Seen set tracking (circular relationship guard)
    let mut unvisited_indices = Vec::new();
    for (idx, record) in rec_list.iter().enumerate() {
        let rec_id = record
            .rec_ids()
            .first()
            .copied()
            .ok_or_else(|| Error::programming("empty record in rec_list"))?;
        let token_id = rec_id
            .origin()
            .map(|r| r.get())
            // A record without an origin is identified by its own id.
            .unwrap_or_else(|| rec_id.get());
        let tracker = tx
            .copy_tracker
            .as_mut()
            .ok_or_else(|| Error::programming("missing copy_tracker on Transaction"))?;
        let already_seen = tracker.record_seen(spec.id, token_id)?;
        if already_seen {
            vals_list.push(None);
        } else {
            // No explicit defaults means an empty override map (CRUD §4.7).
            vals_list.push(Some(default.cloned().unwrap_or_default()));
            unvisited_indices.push(idx);
        }
    }

    if unvisited_indices.is_empty() || fields_to_copy.is_empty() {
        return Ok(vals_list);
    }

    // 4. Batch read field cells per field across all records in the recordset
    for field in &fields_to_copy {
        let all_cells = tx.field_cells(records, field.id).await?;
        if all_cells.len() != n {
            return Err(Error::programming(format!(
                "field_cells for '{}' on '{}' returned {} cells, expected {}",
                field.name,
                records.model_name(),
                all_cells.len(),
                n
            )));
        }

        if matches!(field.ty, erp_fields::FieldType::One2many { .. }) {
            let comodel = crate::relational::comodel_of(&registry, field)?;
            for &idx in &unvisited_indices {
                let cell = &all_cells[idx];
                let ids =
                    extract_ids(std::slice::from_ref(cell), field.name, records.model_name())?;
                let mut sorted_ids = ids.to_vec();
                sorted_ids.sort_unstable();

                let record = rec_list.get(idx).ok_or_else(|| {
                    Error::programming(format!("copy: record index {idx} out of bounds"))
                })?;
                let line_records =
                    Records::empty(record.env().clone(), comodel.id).browse_ids(sorted_ids);
                let line_records = line_records.with_context([("active_test", Value::Bool(false))]);

                // Empty child sets produce an empty command list without descending
                // into another record. Preserve the override call, but do not charge
                // the leaf's empty relation against its allowed nesting depth.
                let entered_level = !line_records.is_empty();
                if entered_level {
                    tx.copy_tracker
                        .as_mut()
                        .ok_or_else(|| Error::programming("missing copy_tracker on Transaction"))?
                        .enter_level()?;
                }
                let lines_copied = line_records.copy_data(tx, None).await;
                if entered_level && let Some(tracker) = tx.copy_tracker.as_mut() {
                    tracker.exit_level();
                }
                let lines_copied = lines_copied?;

                let mut cmds = Vec::new();
                for line in lines_copied.into_iter().flatten() {
                    cmds.push(Value::list([
                        Value::Int(0),
                        Value::Int(0),
                        Value::Map(Arc::new(line)),
                    ]));
                }
                if let Some(vals) = vals_list[idx].as_mut() {
                    vals.insert(field.name.to_owned(), Value::list(cmds));
                }
            }
        } else if matches!(field.ty, erp_fields::FieldType::Many2many { .. }) {
            let comodel = crate::relational::comodel_of(&registry, field)?;
            for &idx in &unvisited_indices {
                let cell = &all_cells[idx];
                let ids =
                    extract_ids(std::slice::from_ref(cell), field.name, records.model_name())?;
                let record = rec_list.get(idx).ok_or_else(|| {
                    Error::programming(format!("copy: record index {idx} out of bounds"))
                })?;
                let targets =
                    Records::empty(record.env().clone(), comodel.id).browse_ids(ids.to_vec());
                let readable_targets = targets.filtered_access(tx, Operation::Read).await?;
                let set_cmd = Command::Set(readable_targets.rec_ids().to_vec()).to_value();
                if let Some(vals) = vals_list[idx].as_mut() {
                    vals.insert(field.name.to_owned(), Value::list([set_cmd]));
                }
            }
        } else if field.translate {
            for &idx in &unvisited_indices {
                let rec = rec_list.get(idx).ok_or_else(|| {
                    Error::programming(format!("copy: record index {idx} out of bounds"))
                })?;
                let id = *rec.rec_ids().first().ok_or_else(|| {
                    Error::programming(format!("copy: record index {idx} has no id"))
                })?;
                let what = format!("copy {}.{}#{id}", spec.name, field.name);
                // loaded by the `field_cells` call above, so an absent slot here is a stored NULL
                let raw_cell = tx.cache.get(spec.id, field.id, id).unwrap_or(Cell::Null);
                let v = match raw_cell {
                    Cell::Null => Value::False,
                    Cell::Json(json_arc) => match json_arc.as_ref() {
                        serde_json::Value::Object(map) => {
                            let mut d = erp_fields::ValueMap::new();
                            for (k, val) in map {
                                let serde_json::Value::String(s) = val else {
                                    return Err(Error::programming(format!(
                                        "{what}: translation for {k} is not a string"
                                    )));
                                };
                                d.insert(k.clone(), Value::str(s));
                            }
                            Value::Map(Arc::new(d))
                        }
                        _ => {
                            return Err(Error::programming(format!(
                                "{what}: translated cell is not a language map"
                            )));
                        }
                    },
                    other => {
                        return Err(Error::programming(format!(
                            "{what}: translated cell has unexpected shape {other:?}"
                        )));
                    }
                };
                if let Some(vals) = vals_list[idx].as_mut() {
                    vals.insert(field.name.to_owned(), v);
                }
            }
        } else {
            for &idx in &unvisited_indices {
                let cell = &all_cells[idx];
                let v =
                    extract_value(std::slice::from_ref(cell), field.name, records.model_name())?;
                if let Some(vals) = vals_list[idx].as_mut() {
                    vals.insert(field.name.to_owned(), v);
                }
            }
        }
    }

    Ok(vals_list)
}

type CopyFn = MethodFn<(Option<ValueMap>,), Records>;
type CopyDataFn = MethodFn<(Option<ValueMap>,), Vec<Option<ValueMap>>>;

fn base_copy_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Option<ValueMap>,),
    _sup: Super<(Option<ValueMap>,), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(async move { base_copy(tx, records, args.0).await })
}

fn copy_entry() -> &'static (dyn Any + Send + Sync) {
    static F: CopyFn = base_copy_impl;
    &F
}

static BASE_COPY: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "copy",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<CopyFn>,
    entry: copy_entry,
    file: file!(),
    line: line!(),
};

fn base_copy_data_impl<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Option<ValueMap>,),
    _sup: Super<(Option<ValueMap>,), Vec<Option<ValueMap>>>,
) -> BoxFuture<'a, Result<Vec<Option<ValueMap>>>> {
    Box::pin(async move { base_copy_data(tx, records, args.0.as_ref()).await })
}

fn copy_data_entry() -> &'static (dyn Any + Send + Sync) {
    static F: CopyDataFn = base_copy_data_impl;
    &F
}

static BASE_COPY_DATA: MethodImpl = MethodImpl {
    addon: "base",
    model: "base",
    method: "copy_data",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<CopyDataFn>,
    entry: copy_data_entry,
    file: file!(),
    line: line!(),
};

/// The `base` implementations of `copy` and `copy_data`, registered by [`crate::Dispatch::build`].
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 2] = [&BASE_COPY, &BASE_COPY_DATA];

#[cfg(test)]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;
    use erp_registry::{AddonDescriptor, ModelDecl, RegistryBuilder};

    #[test]
    fn copy_depth_limit_is_validation() {
        let mut tracker = CopyTracker::new();
        for _ in 0..MAX_COPY_DEPTH {
            tracker.enter_level().unwrap();
        }
        let err = tracker.enter_level().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.to_string(), "copy: nested deeper than 16 levels");
    }

    #[test]
    fn copy_record_limit_is_validation_and_duplicates_do_not_consume_it() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "test_copy_cap",
            depends: &[],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("test_copy_cap", "test_copy_cap.item");
        let registry = RegistryBuilder::new([&ADDON])
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let model = registry.model_id("test_copy_cap.item").unwrap();
        let mut tracker = CopyTracker::new();
        for id in 1..=MAX_COPY_RECORDS {
            assert!(!tracker.record_seen(model, id as i64).unwrap());
        }
        assert!(tracker.record_seen(model, 1).unwrap());
        let err = tracker
            .record_seen(model, MAX_COPY_RECORDS as i64 + 1)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.to_string(), "copy: exceeded limit of 10000 records");
    }
}
