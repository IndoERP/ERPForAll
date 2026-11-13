//! `web_read`, `web_search_read` and `web_name_search` (GRP §4.8).
//!
//! `web_read` reads the specification's fields of the records with one `read` (stored columns in
//! one query per chunk), then expands relational fields level by level: the co-records of every
//! record are read together (one `web_read` per field and level, never per record).

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::rows::{Key, NO_TARGET, RowRef, Rows};
use erp_fields::{Cell, Error, FieldType, RecId, Result, Value, ValueMap};
use rustc_hash::{FxHashMap, FxHashSet};

use super::spec::Spec;
use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::read::Load;
use crate::records::Records;
use crate::rows_build::{Level, Sink};
use crate::search::{NameSearch, SearchOptions};

pub(crate) type Row = ValueMap;

fn map_value(row: Row) -> Value {
    Value::Map(Arc::new(row))
}

pub(crate) fn id_value(id: RecId) -> Value {
    match (id.as_real(), id.origin()) {
        (Some(real), _) => Value::Int(real.get()),
        (None, Some(origin)) => Value::Int(origin.get()),
        (None, None) => Value::False,
    }
}

/// Every name of `spec` must be a field of the records' model (`read` raises otherwise).
pub(crate) fn check_spec(records: &Records, spec: &Spec) -> Result<()> {
    let model = records.spec();
    for name in spec.names() {
        if model.field(name).is_none() {
            return Err(Error::validation(format!(
                "Invalid field '{name}' on model '{}'",
                model.name
            )));
        }
    }
    Ok(())
}

/// `base.web_read` (boxed: it recurses into co-records): one [`Value::Row`] handle per row of
/// the result batch (step 1; the method chain still returns `Vec<Value>` until
/// PERF.ROWS-API).
pub(crate) fn web_read<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    spec: &'a Spec,
) -> BoxFuture<'a, Result<Vec<Value>>> {
    Box::pin(async move { Ok(web_read_batch(tx, records, spec).await?.into_values()) })
}

/// [`web_read`] as one result batch: `id`, then the specification's keys in the client's order,
/// relational keys as child batches by position.
pub(crate) async fn web_read_batch(
    tx: &mut Transaction,
    records: &Records,
    spec: &Spec,
) -> Result<Rows> {
    web_read_level(tx, records, spec).await?.finish()
}

/// [`web_read`] as maps, nested rows as maps and lists of maps (the shape before), for
/// the group reads and the onchange diff, which rewrite rows by key; they move to batches with
/// PERF.ROWS-GROUPS and PERF.ROWS-ONCHANGE (§9).
pub(crate) fn web_read_rows<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    spec: &'a Spec,
) -> BoxFuture<'a, Result<Vec<Row>>> {
    Box::pin(async move {
        let rows = web_read_batch(tx, records, spec).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            out.push(row_to_map(&row)?);
        }
        Ok(out)
    })
}

/// A batch row as a map, its nested rows as maps and lists of maps.
fn row_to_map(row: &RowRef) -> Result<Row> {
    let mut map = Row::with_capacity(row.rows().layout().len());
    for entry in row.iter() {
        let (key, value) = entry?;
        map.insert(key.to_owned(), deep_value(value)?);
    }
    Ok(map)
}

fn deep_value(value: Value) -> Result<Value> {
    Ok(match value {
        Value::Row(row) => map_value(row_to_map(&row)?),
        Value::Rows(rows) => {
            let mut items = Vec::with_capacity(rows.len());
            for row in rows.iter()? {
                items.push(map_value(row_to_map(&row)?));
            }
            Value::list(items)
        }
        other => other,
    })
}

/// One level of [`web_read_batch`] before it is finished (boxed: it recurses into co-records).
fn web_read_level<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    spec: &'a Spec,
) -> BoxFuture<'a, Result<Level>> {
    Box::pin(async move {
        tx.admit_records(records).await?;
        check_spec(records, spec)?;
        let registry = Arc::clone(records.env().registry());
        let model = registry.model_by_id(records.model_id());
        let unsaved = records.rec_ids().iter().any(|id| id.is_new());
        let persisted = records.rec_ids().iter().any(|id| !id.is_new());
        if unsaved
            && persisted
            && spec.names().any(|name| {
                model
                    .field(name)
                    .is_some_and(|field| tx.shared.graph.requires_persisted((model.id, field.id)))
            })
        {
            // Availability belongs to each record, not its siblings. Keep both reads batched
            // and restore the caller's order and absent keys after relational expansion.
            let real =
                records.browse_ids(records.rec_ids().iter().copied().filter(|id| !id.is_new()));
            let new =
                records.browse_ids(records.rec_ids().iter().copied().filter(|id| id.is_new()));
            let real_rows = web_read_level(tx, &real, spec).await?.finish()?;
            let new_rows = web_read_level(tx, &new, spec).await?.finish()?;
            return interleave_available_rows(records, spec, real_rows, new_rows);
        }
        let admitted = if unsaved {
            spec.iter().fold(Spec::new(), |out, (name, item)| {
                if unsaved
                    && model.field(name).is_some_and(|field| {
                        tx.shared.graph.requires_persisted((model.id, field.id))
                    })
                {
                    out
                } else {
                    out.with(name, item.clone())
                }
            })
        } else {
            spec.clone()
        };
        let names: Vec<&str> = admitted.names().collect();
        if names.is_empty() || names == ["id"] {
            return Ok(Level::ids_only(model.name, records.rec_ids()));
        }
        let (present, mut level) = records.read_level(tx, &names, Load::Raw).await?;
        if level.len() == 0 {
            return Ok(level);
        }
        // the records read, in row order (`read` leaves out deleted ones)
        let present = records.browse_ids(present.rec_ids().iter().copied());
        prefetch_comodels(tx, &present, model, &admitted).await?;
        for (name, item) in admitted.iter() {
            let Some(field) = model.field(name) else {
                continue;
            };
            let Some(pos) = level.position(field.name) else {
                return Err(Error::programming(format!(
                    "web_read of {} lost the key '{name}' (internal error)",
                    model.name
                )));
            };
            match &field.ty {
                FieldType::Many2one { .. } => {
                    expand_many2one(tx, &present, &mut level, pos, name, item).await?;
                }
                FieldType::One2many { .. } | FieldType::Many2many { .. } => {
                    expand_x2many(tx, &present, &mut level, pos, name, item).await?;
                }
                FieldType::Reference { .. } | FieldType::Many2oneReference { .. } => {
                    if item.is_empty() {
                        continue;
                    }
                    read_references(tx, &present, &mut level, name, item).await?;
                }
                _ => {}
            }
        }
        Ok(level)
    })
}

/// Reassemble independently authorized homogeneous reads without maps per record. Missing
/// fields remain absent; child row/list values retain their nested batch handles.
fn interleave_available_rows(
    records: &Records,
    spec: &Spec,
    real: Rows,
    new: Rows,
) -> Result<Level> {
    let mut by_id = FxHashMap::default();
    for row in real.iter().chain(new.iter()) {
        let Value::Int(id) = row.get("id")? else {
            return Err(Error::programming(
                "web_read partition lost its raw record id",
            ));
        };
        by_id.insert(id, row);
    }
    // Reads omit deleted records; duplicates and the requested real/new interleaving survive.
    let ids: Vec<RecId> = records
        .rec_ids()
        .iter()
        .copied()
        .filter(|id| by_id.contains_key(&id.get()))
        .collect();
    let model = records.spec();
    let mut out = Level::ids_only(model.name, &ids);
    for name in spec.names().filter(|name| *name != "id") {
        let field = model
            .field(name)
            .ok_or_else(|| Error::programming("web_read partition lost a validated field"))?;
        let mut sink = Sink::for_field(&field.ty, Load::Raw, ids.len());
        let mut present = false;
        for id in &ids {
            let row = by_id
                .get(&id.get())
                .ok_or_else(|| Error::programming("web_read partition lost a selected row"))?;
            if row.contains(name) {
                sink.push_value(row.get(name)?)?;
                present = true;
            } else {
                sink.push_absent()?;
            }
        }
        if present {
            out.push(Key::Field(field.name), sink)?;
        }
    }
    Ok(out)
}

/// A many2one key: without a sub-specification its raw id (a new target as its origin); with
/// one, a child batch of the distinct targets and one position per row, the
/// target's `display_name` as one more key of the child batch.
async fn expand_many2one(
    tx: &mut Transaction,
    present: &Records,
    level: &mut Level,
    pos: usize,
    name: &str,
    item: &super::spec::SpecItem,
) -> Result<()> {
    let len = level.len();
    let Some(sub) = &item.fields else {
        // a new target reads as its origin
        for row in 0..len {
            if let Some(raw) = level.sink(pos)?.int_at(row)?
                && raw < 0
            {
                let value = by_target(present, tx, name, raw).await?;
                level.sink_mut(pos)?.set_value(row, value)?;
            }
        }
        return Ok(());
    };
    let mut co = present.related(tx, name).await?;
    if let Some(ctx) = &item.context {
        co = co.with_context(ctx.iter().cloned());
    }
    let extra = sub.without("display_name");
    let mut child = web_read_level(tx, &co, &extra).await?;
    let co_ids: FxHashMap<i64, RecId> = co.rec_ids().iter().map(|id| (id.get(), *id)).collect();
    // child row of each raw target id; the raw ids, before a new target's id reads as its origin
    let mut by_raw: FxHashMap<i64, u32> = FxHashMap::default();
    by_raw.reserve(child.len());
    let mut raws: Vec<i64> = Vec::with_capacity(child.len());
    for k in 0..child.len() {
        let Some(raw) = child.id_at(k)? else {
            return Err(Error::programming(format!(
                "web_read of {name} gave a co-record row without an id (internal error)"
            )));
        };
        raws.push(raw);
        by_raw.insert(raw, row_index(k)?);
        if let Some(target) = co_ids.get(&raw) {
            child.sink_mut(0)?.set_value(k, id_value(*target))?;
        }
    }
    if sub.contains("display_name") {
        let names = tx.display_names_for(&co.engine_sudo(), co.env()).await?;
        let by_target: FxHashMap<i64, &Cell> = co
            .rec_ids()
            .iter()
            .zip(&names)
            .map(|(id, cell)| (id.get(), cell))
            .collect();
        let mut sink = Sink::text(child.len());
        for raw in &raws {
            match by_target.get(raw) {
                Some(cell) => sink.push_cell((*cell).clone())?,
                // no label for that row: the key is not in its dict
                None => sink.push_absent()?,
            }
        }
        child.push(Key::Field("display_name"), sink)?;
    }
    let mut at: Vec<u32> = Vec::with_capacity(len);
    for row in 0..len {
        let target = match level.sink(pos)?.int_at(row)? {
            Some(raw) => match by_raw.get(&raw) {
                // a target whose id reads `False` (a new record without origin) reads `False`
                Some(k) if child.id_at(*k as usize)?.is_some_and(|id| id != 0) => *k,
                _ => NO_TARGET,
            },
            None => NO_TARGET,
        };
        at.push(target);
    }
    level.sink_mut(pos)?.set_one(child.finish()?, at)
}

async fn expand_x2many(
    tx: &mut Transaction,
    present: &Records,
    level: &mut Level,
    pos: usize,
    name: &str,
    item: &super::spec::SpecItem,
) -> Result<()> {
    let len = level.len();
    let all = present.related(tx, name).await?;
    // Originless children are omitted in both raw and expanded web-read formats.
    drop_originless(level.sink_mut(pos)?, len)?;
    let mut co = all.browse_ids(all.rec_ids().iter().copied().filter(has_identity));
    if let Some(order) = &item.order {
        let ids: Vec<i64> = co.ids();
        let sorted = co
            .with_context([("active_test", Value::Bool(false))])
            .search_with(
                tx,
                &Domain::field("id")?.in_(ids),
                &SearchOptions::new().order(order.clone()),
            )
            .await?;
        let rank: FxHashMap<i64, usize> = sorted
            .rec_ids()
            .iter()
            .enumerate()
            .map(|(i, id)| (id.get(), i))
            .collect();
        let sink = level.sink_mut(pos)?;
        for row in 0..len {
            let Some(ids) = sink.ids_at(row)? else {
                continue;
            };
            let mut kept: Vec<(usize, RecId)> = ids
                .iter()
                .filter_map(|id| {
                    let origin_id = origin_of(*id)?.get();
                    rank.get(&origin_id).map(|r| (*r, *id))
                })
                .collect();
            kept.sort_by_key(|(r, _)| *r);
            let kept: Arc<[RecId]> = kept.into_iter().map(|(_, id)| id).collect();
            sink.set_ids(row, kept)?;
        }
        let mut sorted_co: Vec<RecId> = co.rec_ids().to_vec();
        sorted_co.sort_by_key(|id| {
            origin_of(*id)
                .and_then(|o| rank.get(&o.get()).copied())
                .unwrap_or(usize::MAX)
        });
        co = co.browse_ids(sorted_co);
    }
    if let Some(ctx) = &item.context {
        co = co.with_context(ctx.iter().cloned());
    }
    let Some(sub) = &item.fields else {
        // Raw x2many: normalize relation output to origins.
        let sink = level.sink_mut(pos)?;
        for row in 0..len {
            // a row of real ids is already in its final form: no new list for it
            if let Some(ids) = sink.ids_at(row)?
                && ids.iter().any(|id| id.is_new())
            {
                let normalized: Vec<RecId> = ids
                    .iter()
                    .filter_map(|id| origin_of(*id))
                    .map(RecId::real)
                    .collect();
                sink.set_ids(row, Arc::from(normalized))?;
            }
        }
        return Ok(());
    };
    if let Some(limit) = item.limit {
        let limit = usize::try_from(limit).map_err(|_| {
            Error::validation(format!("web_read: limit {limit} of '{name}' is too large"))
        })?;
        let mut seen = FxHashSet::default();
        let mut wanted: Vec<RecId> = Vec::new();
        let sink = level.sink(pos)?;
        for row in 0..len {
            let Some(ids) = sink.ids_at(row)? else {
                continue;
            };
            for id in ids.iter().take(limit) {
                if seen.insert(*id) {
                    wanted.push(*id);
                }
            }
        }
        co = co.browse_ids(wanted);
    }
    let mut child = web_read_level(tx, &co, sub).await?;
    let co_ids: FxHashMap<i64, RecId> = all.rec_ids().iter().map(|id| (id.get(), *id)).collect();
    let mut by_raw: FxHashMap<i64, u32> = FxHashMap::default();
    by_raw.reserve(child.len());
    for k in 0..child.len() {
        let Some(raw) = child.id_at(k)? else {
            return Err(Error::programming(format!(
                "web_read of {name} gave a co-record row without an id (internal error)"
            )));
        };
        by_raw.insert(raw, row_index(k)?);
        if let Some(target) = co_ids.get(&raw) {
            child.sink_mut(0)?.set_value(k, id_value(*target))?;
        }
    }
    let mut offsets: Vec<u32> = Vec::with_capacity(len + 1);
    offsets.push(0);
    let mut at: Vec<u32> = Vec::with_capacity(child.len());
    let sink = level.sink(pos)?;
    for row in 0..len {
        if let Some(ids) = sink.ids_at(row)? {
            for id in ids.iter() {
                let raw = id.get();
                let k = match by_raw.get(&raw) {
                    Some(k) => *k,
                    None => {
                        // a line that was not read gets here: limited out, or left out by
                        // `web_read_level` (deleted or unreadable); never an originless line,
                        // `drop_originless` removed those from every row above
                        let origin = origin_of(*id).ok_or_else(|| {
                            Error::programming(format!(
                                "web_read of {name} reached an originless child line in the \
                                 expanded x2many (internal error)"
                            ))
                        })?;
                        let k = child.push_id_only(origin.get())?;
                        by_raw.insert(raw, k);
                        k
                    }
                };
                at.push(k);
            }
        }
        offsets.push(row_index(at.len())?);
    }
    // one2many lines read in parent order are contiguous: no index list is needed
    let identity = at.len() == child.len() && at.iter().enumerate().all(|(i, k)| *k as usize == i);
    let child = child.finish()?;
    let at = (!identity).then_some(at);
    level.sink_mut(pos)?.set_many(child, offsets, at)
}

/// The persisted identity of a line: its own id when real, else its origin.
fn origin_of(id: RecId) -> Option<erp_fields::RealId> {
    id.as_real().or(id.origin())
}

/// A line with a persisted identity (a real record, or a new one with an origin).
fn has_identity(id: &RecId) -> bool {
    origin_of(*id).is_some()
}

/// Remove the originless new lines from every row of an x2many sink; a row without any keeps
/// its list.
fn drop_originless(sink: &mut Sink, len: usize) -> Result<()> {
    for row in 0..len {
        if let Some(ids) = sink.ids_at(row)?
            && !ids.iter().all(has_identity)
        {
            let kept: Vec<RecId> = ids.iter().copied().filter(has_identity).collect();
            sink.set_ids(row, Arc::from(kept))?;
        }
    }
    Ok(())
}

/// A row position as `u32` (a batch holds at most `u32::MAX - 1` rows).
fn row_index(k: usize) -> Result<u32> {
    u32::try_from(k)
        .ok()
        .filter(|k| *k != NO_TARGET)
        .ok_or_else(|| Error::programming(format!("a web_read level of {k} rows is too large")))
}

/// Fetch what the expansion below reads from the specification's comodels with the statements of
/// all of them in one flight (design note §4): the loop reads one comodel after the other,
/// each waiting for its own answer. The statements are the ones the loop would send (same fields,
/// same records, same access checks), so the statement count is unchanged and the loop then finds
/// everything cached. A relational field the loop reads through a search first (`order`, `limit`
/// on a x2many) is left to the loop, as is a second field to a model already prefetched.
async fn prefetch_comodels(
    tx: &mut Transaction,
    present: &Records,
    model: &erp_registry::ModelSpec,
    spec: &Spec,
) -> Result<()> {
    let mut wanted: Vec<(Records, Vec<String>)> = Vec::new();
    for (name, item) in spec.iter() {
        let Some(field) = model.field(name) else {
            continue;
        };
        let Some(sub) = &item.fields else {
            continue;
        };
        let inner = match &field.ty {
            FieldType::Many2one { .. } => sub.without("display_name"),
            FieldType::One2many { .. } | FieldType::Many2many { .. }
                if item.order.is_none() && item.limit.is_none() =>
            {
                sub.clone()
            }
            // other fields (scalars, references, x2many read through a search) expand elsewhere
            _ => continue,
        };
        let requested: Vec<&str> = inner.names().collect();
        if requested.is_empty() || requested == ["id"] {
            continue;
        }
        let mut co = present.related(tx, name).await?;
        if let Some(ctx) = &item.context {
            co = co.with_context(ctx.iter().cloned());
        }
        // an invalid specification is reported by the loop below, in its own words
        if check_spec(&co, &inner).is_err() {
            continue;
        }
        let has_new = co.rec_ids().iter().any(|id| id.is_new());
        let names: Vec<String> = inner
            .names()
            .filter(|name| {
                !has_new
                    || !co.spec().field(name).is_some_and(|field| {
                        tx.shared
                            .graph
                            .requires_persisted((co.model_id(), field.id))
                    })
            })
            .map(str::to_owned)
            .collect();
        // This is only prefetch: mixed targets' persisted values are read by the recursive
        // per-record availability path. Never turn an empty name list into fetch-all.
        if names.is_empty() || names == ["id"] {
            continue;
        }
        if wanted.iter().any(|(r, _)| r.model_id() == co.model_id()) {
            continue;
        }
        wanted.push((co, names));
    }
    if wanted.len() < 2 {
        return Ok(());
    }
    let requests: Vec<(&Records, Vec<&str>)> = wanted
        .iter()
        .map(|(co, names)| (co, names.iter().map(String::as_str).collect()))
        .collect();
    let requests: Vec<(&Records, &[&str])> = requests
        .iter()
        .map(|(co, names)| (*co, names.as_slice()))
        .collect();
    crate::search::fetch_together(tx, &requests).await
}

/// The origin of a new many2one target (or JSON `null` for originless new record).
async fn by_target(present: &Records, tx: &mut Transaction, name: &str, raw: i64) -> Result<Value> {
    let targets = present.related(tx, name).await?;
    let target = targets.rec_ids().iter().find(|id| id.get() == raw);
    match target {
        Some(id) => match (id.as_real(), id.origin()) {
            (Some(real), _) => Ok(Value::Int(real.get())),
            (None, Some(origin)) => Ok(Value::Int(origin.get())),
            (None, None) => Ok(Value::Json(Arc::new(erp_fields::serde_json::Value::Null))),
        },
        // the level read this raw id from `name`, so the relation must hold it
        None => Err(Error::programming(format!(
            "web_read of {name}: the new target {raw} is not in the relation (internal error)"
        ))),
    }
}

async fn read_references(
    tx: &mut Transaction,
    present: &Records,
    level: &mut Level,
    name: &str,
    item: &super::spec::SpecItem,
) -> Result<()> {
    let registry = Arc::clone(present.env().registry());
    let model = registry.model_by_id(present.model_id());
    let Some(field) = model.field(name) else {
        return Ok(());
    };
    let Some(pos) = level.position(field.name) else {
        return Err(Error::programming(format!(
            "web_read of {} lost the key '{name}' (internal error)",
            model.name
        )));
    };
    let model_field: Option<&'static str> = match &field.ty {
        FieldType::Many2oneReference { model_field } => {
            Some(model.field(model_field).map(|f| f.name).ok_or_else(|| {
                Error::programming(format!(
                    "{}.{name} names the unknown model field '{model_field}'",
                    model.name
                ))
            })?)
        }
        _ => None,
    };
    // (row index, target model, target id)
    let mut targets: Vec<(usize, &'static str, i64)> = Vec::new();
    let values = present.cells(tx, name).await?;
    let models = match model_field {
        Some(mf) => Some(present.cells(tx, mf).await?),
        None => None,
    };
    for (i, cell) in values.iter().enumerate() {
        let target = match (&field.ty, cell) {
            (FieldType::Reference { .. }, Cell::Str(text)) => {
                erp_fields::convert::parse_reference(text).map(|(m, id)| (m.to_owned(), id))
            }
            (FieldType::Many2oneReference { .. }, Cell::Id(id)) if *id != 0 => {
                match models.as_ref().and_then(|m| m.get(i)) {
                    Some(Cell::Str(m)) if !m.is_empty() => Some((m.to_string(), *id)),
                    _ => {
                        // no model: the value is cleared
                        level.sink_mut(pos)?.set_value(i, Value::False)?;
                        None
                    }
                }
            }
            _ => None,
        };
        if let Some((m, id)) = target {
            let spec = registry.model(&m).ok_or_else(|| {
                Error::programming(format!(
                    "{}.{name} refers to the unknown model '{m}'",
                    model.name
                ))
            })?;
            targets.push((i, spec.name, id));
        }
    }
    let mut by_model: Vec<(&'static str, Vec<i64>)> = Vec::new();
    for (_, m, id) in &targets {
        match by_model.iter_mut().find(|(n, _)| n == m) {
            Some((_, ids)) => {
                if !ids.contains(id) {
                    ids.push(*id);
                }
            }
            None => by_model.push((m, vec![*id])),
        }
    }
    enum Resolved {
        Missing,
        Keep,
        Read(Value),
    }
    let mut resolved: FxHashMap<(&'static str, i64), Resolved> = FxHashMap::default();
    for (m, ids) in by_model {
        let mut co = present.env().records(m)?.browse(ids.iter().copied())?;
        if let Some(ctx) = &item.context {
            co = co.with_context(ctx.iter().cloned());
        }
        let existing: FxHashSet<i64> = co.exists(tx).await?.ids().into_iter().collect();
        let mut read: FxHashMap<i64, Row> = FxHashMap::default();
        if let Some(sub) = &item.fields {
            // a refused company context is an error, never "no access to this record"
            // (review F1/S5)
            co.env().companies()?;
            let alive = co.browse(ids.iter().copied().filter(|i| existing.contains(i)))?;
            match web_read_rows(tx, &alive, sub).await {
                Ok(co_rows) => {
                    for row in co_rows {
                        if let Some(Value::Int(i)) = row.get("id") {
                            read.insert(*i, row);
                        }
                    }
                }
                Err(e) if e.kind() == erp_fields::ErrorKind::Access => {
                    for i in alive.ids() {
                        let one = alive.browse([i])?;
                        match web_read_rows(tx, &one, sub).await {
                            Ok(rows) => {
                                for row in rows {
                                    read.insert(i, row);
                                }
                            }
                            Err(e) if e.kind() == erp_fields::ErrorKind::Access => {
                                let mut row = Row::new();
                                row.insert("id".to_owned(), Value::Int(i));
                                row.insert(
                                    "display_name".to_owned(),
                                    Value::str("You don't have access to this record"),
                                );
                                read.insert(i, row);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        for i in ids {
            let value = if !existing.contains(&i) {
                Resolved::Missing
            } else if item.fields.is_some() {
                let mut row = read.remove(&i).unwrap_or_default();
                if matches!(field.ty, FieldType::Reference { .. }) {
                    let mut id = Row::new();
                    id.insert("id".to_owned(), Value::Int(i));
                    id.insert("model".to_owned(), Value::str(m));
                    row.insert("id".to_owned(), map_value(id));
                } else {
                    row.insert("id".to_owned(), Value::Int(i));
                }
                Resolved::Read(map_value(row))
            } else {
                Resolved::Keep
            };
            resolved.insert((m, i), value);
        }
    }
    for (i, m, id) in targets {
        match resolved.get(&(m, id)) {
            Some(Resolved::Missing) | None => {
                level.sink_mut(pos)?.set_value(i, Value::False)?;
                if let Some(mf) = model_field {
                    level
                        .sink_or_absent(Key::Field(mf))?
                        .set_value(i, Value::False)?;
                }
            }
            Some(Resolved::Keep) => {}
            Some(Resolved::Read(value)) => {
                level.sink_mut(pos)?.set_value(i, value.clone())?;
            }
        }
    }
    Ok(())
}

/// `base.web_search_read`.
pub(crate) async fn web_search_read(
    tx: &mut Transaction,
    records: &Records,
    args: super::WebSearchRead,
) -> Result<Value> {
    check_spec(records, &args.specification)?;
    let mut opts = SearchOptions::new().fetch_fields(args.specification.names().map(str::to_owned));
    opts.offset = args.offset;
    opts.limit = args.limit;
    opts.order = args.order.clone();
    let found = records.search_with(tx, &args.domain, &opts).await?;
    let values = web_read_batch(tx, &found, &args.specification).await?;
    let mut out = ValueMap::new();
    if values.is_empty() {
        out.insert("length".to_owned(), Value::Int(0));
        out.insert("records".to_owned(), Value::list([]));
        return Ok(map_value(out));
    }
    let current = values.len() as u64 + args.offset;
    let limit_reached = args.limit == Some(values.len() as u64);
    let force = records
        .env()
        .context()
        .get("force_search_count")
        .is_some_and(|v| v.is_truthy());
    let count_limit_reached = args.count_limit.is_some_and(|c| c > 0 && c <= current);
    let length = if args.limit.is_some_and(|l| l > 0)
        && ((limit_reached && !count_limit_reached) || force)
    {
        records
            .search_count(tx, &args.domain, args.count_limit)
            .await?
    } else {
        current
    };
    out.insert("length".to_owned(), Value::Int(length as i64));
    out.insert("records".to_owned(), Value::from(values));
    Ok(map_value(out))
}

/// `base.web_name_search`.
pub(crate) async fn web_name_search(
    tx: &mut Transaction,
    records: &Records,
    args: super::WebNameSearch,
) -> Result<Vec<Value>> {
    check_spec(records, &args.specification)?;
    let options = NameSearch::new()
        .domain(args.domain.clone())
        .operator(args.operator)
        .limit(args.limit);
    let pairs = records.name_search_with(tx, &args.name, &options).await?;
    let found = records.browse(pairs.iter().map(|(id, _)| *id))?;
    let mut level = web_read_level(tx, &found, &args.specification).await?;
    if args.specification.contains("display_name") {
        let formatted = found
            .with_context([("formatted_display_name", Value::Bool(true))])
            .cells(tx, "display_name")
            .await?;
        // one formatted name per record read: the rows are exactly the records found (`read`
        // keeps every one of them, they exist), so another count is an internal error
        if formatted.len() != level.len() {
            return Err(Error::programming(format!(
                "web_name_search read {} rows but {} formatted names (internal error)",
                level.len(),
                formatted.len()
            )));
        }
        let mut sink = Sink::text(level.len());
        for cell in formatted {
            sink.push_cell(cell)?;
        }
        level.push(Key::Field("__formatted_display_name"), sink)?;
    }
    Ok(level.finish()?.into_values())
}
