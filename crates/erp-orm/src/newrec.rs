//! Relational fields of new (in-memory) records, and x2many commands on new records and on
//! non-stored x2many fields (ENV §4.9, REL §4.8.6–4.8.8;/045,…058,
//! -061, -081, -083).
//!
//! - **Values.** A new record's many2one holds a database id or a new record's id. Its x2many
//!   holds new records: a new record with an origin starts from the origin's value, every
//!   linked record mapped to the new record of the same origin (`line.new(origin=line)` is that
//!   same record), so `new.line_ids._origin == origin.line_ids`; the origins' values are read in
//!   one query for every uncached new record of the prefetch set.
//! - **Commands** on new records edit the cache only (nothing is written to the database): a
//!   `CREATE` makes a new comodel record, `UPDATE` changes a new line's values, `DELETE` and
//!   `UNLINK` drop the line from the value without deleting anything, `LINK`/`SET` link database
//!   records as new records with those origins. The inverse fields of the lines follow: a
//!   one2many line's many2one points at the new record, a many2many line's mirror lists it.
//!   Database records are never patched with new records.
//! - **Patches.** When the x2many to patch on a new record is not in the cache yet (it would have
//!   to be read from its origin), the change is kept as a patch and applied when the value is
//!   read (linking costs no query).
//! - **Non-stored x2many fields** take commands on their current (computed) value, in the cache;
//!   the field's inverse method then persists the result, and the value is dropped from the
//!   cache so that new lines do not outlive it (REL §4.8.8).
//! - **Back to commands** (`convert_to_write`): an x2many value holding new records becomes
//!   `SET` of the database records (origins included), then `CREATE` for each new line without
//!   origin and `UPDATE` for each new line whose values differ from its origin's, the inverse
//!   fields left out. Writing such a value on database records converts it
//!   the same way, so new lines assigned to a database record are created.
//! - **Company-dependent fields** keep a company map in the new record's cache, as stored records
//!   do: an explicit value or a default is the environment's company entry, an origin's
//!   map is the start value, and a read projects the reading environment's company. Write commands
//!   carry that company's scalar.

use std::sync::Arc;

use erp_fields::{Cell, Error, ErrorKind, FieldType, RecId, Result, Storage};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::access::Operation;
use crate::commands::Command;
use crate::env::Env;
use crate::records::Records;
use crate::vals::RawVals;

/// The id a new record's x2many holds for a comodel record: new records as they are, database
/// records as the new record of that origin.
pub(crate) fn align(id: RecId) -> Result<RecId> {
    match id.as_real() {
        None => Ok(id),
        Some(real) => RecId::new_from_origin(real).ok_or_else(|| {
            Error::programming(format!("record id {real} out of the new-record range"))
        }),
    }
}

fn ids_in(cell: &Cell) -> Vec<RecId> {
    match cell {
        Cell::Ids(ids) => ids.to_vec(),
        Cell::Id(_) => crate::relational::id_of(cell).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Append `id` unless present. Linear in the list: x2many values of new records are form-sized
/// (lines of one document); `extend_unique` serves batches.
fn push_unique(list: &mut Vec<RecId>, id: RecId) {
    if !list.contains(&id) {
        list.push(id);
    }
}

/// Append the `ids` not present yet, in order (one hash set for the batch).
fn extend_unique(list: &mut Vec<RecId>, ids: impl IntoIterator<Item = RecId>) {
    let mut seen: FxHashSet<RecId> = list.iter().copied().collect();
    for id in ids {
        if seen.insert(id) {
            list.push(id);
        }
    }
}

/// Fields `convert_to_write` reproduces: stored values that `create`/`write` accept (not the id,
/// the audit fields, readonly computed fields, or the inverse fields of the x2many).
fn writable(field: &FieldSpec, skip: &[FieldId]) -> bool {
    if field.magic
        || field.is_generated()
        || field.server_set
        || matches!(field.ty, FieldType::Id)
        || skip.contains(&field.id)
    {
        return false;
    }
    // what `create`/`write` takes: stored fields that are not readonly computes, and non-stored
    // fields that save themselves (an inverse method, an editable related field)
    let saves = field.inverse.is_some() || (field.related.is_some() && !field.readonly);
    if field.store {
        !field.is_computed() || !field.readonly || saves
    } else {
        saves
    }
}

impl Transaction {
    // ---- new records -----------------------------------------------------------------------------

    /// Originless many2one inputs must name active new records of the declared comodel
    /// in this transaction. Failed construction and rolled-back scopes revoke identities;
    /// allocation-counter position alone is not admission. New IDs cannot be persisted.
    ///
    /// `Model.new(values, origin, ref)`: an in-memory record (ENV §4.9). No SQL is written; the
    /// values are put in the cache (x2many values as command lists, applied in new-record mode);
    /// other fields are read from the origin, computed or defaulted on access. New records with
    /// the same origin (or the same `reference`) are the same record. Explicit generated output
    /// inputs return `Validation`; direct reads of generated outputs on any new record return
    /// `Programming` because only a persisted record has an authoritative value.
    pub async fn new_record(
        &mut self,
        model: &Records,
        vals: RawVals,
        origin: Option<RecId>,
        reference: Option<&str>,
    ) -> Result<Records> {
        self.refuse_adapter_effect()?;
        self.admit_records(model).await?;
        let spec = model.spec();
        crate::database_generated::check_write_names(self, spec, vals.names())?;
        if !spec.has_table() {
            return Err(Error::programming(format!(
                "Cannot create new records of abstract model {}",
                spec.name
            )));
        }
        // every value is checked before anything is cached
        let mut plain: Vec<(&FieldSpec, Cell)> = Vec::new();
        let mut x2many: Vec<(FieldId, Vec<Command>)> = Vec::new();
        for (name, cell) in vals.iter() {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!("Invalid field '{name}' on model '{}'", spec.name))
            })?;
            if name == "id" {
                continue;
            }
            if field.inherited.is_some() {
                return Err(inherited_on_new(spec, name));
            }
            if field.ty.is_x2many() {
                // a plain id list is a SET (REL §4.8.2)
                let commands = match cell {
                    Cell::Ids(ids) => vec![Command::Set(ids.to_vec())],
                    Cell::Null => vec![Command::Clear],
                    other => {
                        return Err(Error::validation(format!(
                            "Wrong value for {}.{name}: {other:?}",
                            spec.name
                        )));
                    }
                };
                x2many.push((field.id, commands));
                continue;
            }
            let admitted = if matches!(field.ty, FieldType::Html) {
                self.admit_html_cell(spec, field, cell.clone())?
            } else {
                new_record_cell(spec, field, cell.clone(), self)?
            };
            plain.push((field, admitted));
        }
        for (name, commands) in vals.iter_commands() {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!("Invalid field '{name}' on model '{}'", spec.name))
            })?;
            if !field.ty.is_x2many() {
                return Err(Error::programming(format!(
                    "Commands given for {}.{name}, which is not a one2many or many2many field",
                    spec.name
                )));
            }
            x2many.push((field.id, commands.to_vec()));
        }
        // Keep the allocation boundary, not an Arc snapshot per record: cloning here
        // would force a full membership-map copy on every successful allocation.
        let counter_before = self.engine.next_new;
        let mut new_ref: Option<String> = None;
        let id = match (origin, reference) {
            (Some(o), _) => {
                let real = o.as_real().or(o.origin()).ok_or_else(|| {
                    Error::programming("the origin of a new record must be a database record")
                })?;
                RecId::new_from_origin(real)
                    .ok_or_else(|| Error::programming(format!("origin id {real} out of range")))?
            }
            (None, Some(r)) => match self.engine.refs.get(r) {
                Some(id) => *id,
                None => {
                    let id = self.next_new_id()?;
                    self.engine.refs_mut().insert(r.to_owned(), id);
                    new_ref = Some(r.to_owned());
                    id
                }
            },
            (None, None) => self.next_new_id()?,
        };
        let was_active = self.engine.new_records.contains_key(&id);
        if id.unique_index().is_some() {
            if self
                .engine
                .new_records
                .get(&id)
                .is_some_and(|owner| *owner != spec.id)
            {
                return Err(Error::validation(format!(
                    "New record reference on {} belongs to a different model",
                    spec.name
                )));
            }
            Arc::make_mut(&mut self.engine.new_records).insert(id, spec.id);
        }
        let record = model.browse_ids([id]);
        let result = self.fill_new_record(&record, plain, x2many).await;
        if result.is_err() {
            // Nested new() calls may have introduced other identities before failure.
            // Revoke that allocation range only on this error path; preexisting identities
            // (including a reference being refilled) keep their admission.
            let active = Arc::make_mut(&mut self.engine.new_records);
            active.retain(|new_id, _| new_id.unique_index().is_some_and(|n| n <= counter_before));
            if !was_active {
                active.remove(&id);
            }
            // a failed new() leaves no partial record behind: its cached values, patches and a
            // reference it introduced are dropped
            let model_id = model.model_id();
            self.cache.model_mut(model_id).invalidate(None, Some(&[id]));
            self.engine
                .context_mut()
                .invalidate(model_id, None, Some(&[id]));
            self.engine
                .patches_mut()
                .retain(|(m, _, r), _| !(*m == model_id && *r == id));
            if let Some(r) = new_ref {
                self.engine.refs_mut().remove(&r);
            }
        }
        result.map(|()| record)
    }

    /// Cache the checked values of new record `record` (`new_record`).
    async fn fill_new_record(
        &mut self,
        record: &Records,
        mut plain: Vec<(&FieldSpec, Cell)>,
        x2many: Vec<(FieldId, Vec<Command>)>,
    ) -> Result<()> {
        let model_id = record.model_id();
        let Some(&id) = record.rec_ids().first() else {
            return Ok(());
        };
        let initial = id.origin().is_none()
            && record
                .spec()
                .fields
                .iter()
                .all(|field| !self.cache.contains(model_id, field.id, id));
        let fields: Vec<FieldId> = plain
            .iter()
            .filter(|(f, _)| matches!(f.ty, FieldType::Monetary { .. }))
            .map(|(f, _)| f.id)
            .collect();
        if !fields.is_empty() {
            let budget = crate::monetary::StagingBudget::new(1);
            budget.check_grid(1, fields.len())?;
            let shared = Arc::clone(&self.shared);
            let slots = crate::monetary::staging_slots(
                record.spec(),
                shared.graph.model(model_id),
                &fields,
            );
            let mut rows = vec![vec![None; slots.len()]];
            for (field, cell) in &plain {
                if let Ok(slot) = slots.binary_search(&field.id) {
                    rows[0][slot] = Some(cell.clone());
                }
            }
            crate::monetary::normalize(
                self,
                record.env(),
                crate::monetary::Stage {
                    initial,
                    spec: record.spec(),
                    ids: Some(record.rec_ids()),
                    fields: &slots,
                    rows: &mut rows,
                    included: None,
                    budget: &budget,
                    precharged: None,
                    kind: crate::monetary::StageKind::Direct,
                },
                &fields,
                false,
                true,
            )
            .await?;
            for (field, cell) in &mut plain {
                if fields.contains(&field.id) {
                    *cell = slots
                        .binary_search(&field.id)
                        .ok()
                        .and_then(|slot| rows[0][slot].take())
                        .ok_or_else(|| {
                            Error::programming("Monetary new-record staging lost an input")
                        })?;
                }
            }
        }
        let mut many2ones: Vec<(FieldId, Cell)> = Vec::new();
        for (field, mut cell) in plain {
            if field.company_dependent {
                // a map holds no single target: no inverse one2many lists the draft
                cell = self.draft_property_map(record, field, &cell).await?;
            } else if matches!(field.ty, FieldType::Many2one { .. }) {
                many2ones.push((field.id, cell.clone()));
            }
            let slot = self.slot(record.env(), model_id, field.id)?;
            self.slot_set(&slot, model_id, field.id, [(id, cell)])?;
        }
        // the inverse one2many values of new targets list the new record (REL §4.4.4)
        for (field, cell) in many2ones {
            let old = vec![Some(Cell::Null)];
            self.update_inverses(model_id, field, record, &old, &cell)
                .await?;
        }
        for (field, commands) in x2many {
            self.write_x2many_new(model_id, field, vec![(record.clone(), commands)])
                .await?;
        }
        Ok(())
    }

    pub(crate) fn next_new_id(&mut self) -> Result<RecId> {
        self.engine.next_new += 1;
        RecId::new_unique(self.engine.next_new)
            .ok_or_else(|| Error::programming("too many new records in one transaction"))
    }

    // ---- x2many values of new records -----------------------------------------------------------

    /// Cache the x2many values of the new records `ids` (none cached): the origins' values as new
    /// records of those origins (one read for all origins), plus the pending patches.
    pub(crate) async fn ensure_new_x2many(
        &mut self,
        records: &Records,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<()> {
        let model = records.model_id();
        let registry = Arc::clone(records.env().registry());
        let fs = registry.model_by_id(model).field_by_id(field);
        let requested: Vec<RecId> = ids
            .iter()
            .copied()
            .filter(|i| i.origin().is_some())
            .collect();
        // The uncached new records of the prefetch set read their origins in the same query, at
        // most one prefetch chunk of them.
        let limit = self.config().prefetch_chunk.max(1).max(requested.len());
        let mut seen: FxHashSet<RecId> = requested.iter().copied().collect();
        let mut extras: Vec<RecId> = Vec::new();
        for id in records.prefetch_ids() {
            if requested.len() + extras.len() >= limit {
                break;
            }
            if id.origin().is_some() && !self.cache.contains(model, field, *id) && seen.insert(*id)
            {
                extras.push(*id);
            }
        }
        let mut with_origin = requested.clone();
        let mut values: FxHashMap<RecId, Vec<RecId>> = FxHashMap::default();
        if fs.store && !(requested.is_empty() && extras.is_empty()) {
            let extras = self
                .check_origins(records.env(), model, &requested, &extras)
                .await?;
            with_origin.extend(extras);
            let origins: Vec<RecId> = with_origin
                .iter()
                .filter_map(|i| i.origin().map(RecId::real))
                .collect();
            let origins_arc: Arc<[RecId]> = Arc::from(origins);
            let origin_records = Records::from_parts(
                records.env().engine_sudo(),
                model,
                Arc::clone(&origins_arc),
                origins_arc,
            );
            self.ensure_x2many(&origin_records, field).await?;
            for id in &with_origin {
                let Some(origin) = id.origin() else { continue };
                // the field is stored and `ensure_x2many` just cached it for every origin: a
                // missing value is an engine bug, not an empty value
                let cell = self
                    .cache
                    .get(model, field, RecId::real(origin))
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "x2many value of {}.{} not cached for origin {origin}",
                            registry.model_by_id(model).name,
                            fs.name
                        ))
                    })?;
                let mapped = ids_in(&cell)
                    .into_iter()
                    .map(align)
                    .collect::<Result<Vec<_>>>()?;
                values.insert(*id, mapped);
            }
        }
        let storage = fs.storage();
        let mut targets: Vec<RecId> = ids.to_vec();
        for id in &with_origin {
            if !targets.contains(id) {
                targets.push(*id);
            }
        }
        for id in targets {
            let mut value = values.remove(&id).unwrap_or_default();
            if let Some(patch) = self.engine.patches_mut().remove(&(model, field, id)) {
                extend_unique(&mut value, patch);
            }
            self.cache
                .model_mut(model)
                .set(field, storage, id, Cell::Ids(value.into()), false)?;
        }
        Ok(())
    }

    /// Readability goes through the remembered rule decisions
    /// ([`Transaction::readable_ids`]), the requested origins and the prefetch extras' in one
    /// call; the first requested origin the rules hide is an `Access` error, a deleted one
    /// `Missing`.
    async fn check_origins(
        &mut self,
        env: &Env,
        model: ModelId,
        requested: &[RecId],
        extras: &[RecId],
    ) -> Result<Vec<RecId>> {
        if env.su() || (requested.is_empty() && extras.is_empty()) {
            return Ok(extras.to_vec());
        }
        let origin_of = |i: &RecId| i.origin().map(RecId::real);
        let requested_origins: Vec<RecId> = requested.iter().filter_map(origin_of).collect();
        let mut all = requested_origins.clone();
        all.extend(extras.iter().filter_map(origin_of));
        // the policy is current before the model check decides (SM-3)
        // a refused company context is an error, never "fill nothing" (review F1/S5)
        env.companies()?;
        self.prepare_access(env).await?;
        if requested_origins.is_empty() {
            if let Err(error) = self.check_model(env, model, Operation::Read) {
                if error.kind() == ErrorKind::Access {
                    return Ok(Vec::new());
                }
                return Err(error);
            }
        } else {
            self.check_model(env, model, Operation::Read)?;
        }
        let visible: FxHashSet<RecId> = self
            .readable_ids(env, model, &all)
            .await?
            .into_iter()
            .collect();
        for origin in &requested_origins {
            if visible.contains(origin) {
                continue;
            }
            let ids: Arc<[RecId]> = Arc::from(&[*origin][..]);
            let co = Records::from_parts(env.clone(), model, Arc::clone(&ids), ids);
            let one: Vec<erp_fields::RealId> = origin.as_real().into_iter().collect();
            if let Some(error) = self
                .record_access_error(env, model, Operation::Read, &one)
                .await?
            {
                return Err(error);
            }
            return Err(crate::read::missing_error(&co, *origin));
        }
        Ok(extras
            .iter()
            .filter(|e| origin_of(e).is_some_and(|o| visible.contains(&o)))
            .copied()
            .collect())
    }

    /// Keep `lines` as a patch of the x2many value of new record `record`, applied when that
    /// value is read.
    pub(crate) fn patch_new_x2many(
        &mut self,
        model: ModelId,
        field: FieldId,
        record: RecId,
        lines: &[RecId],
    ) -> Result<()> {
        self.add_to_new_x2many(model, field, record, lines)
    }

    /// Add `lines` to the cached x2many value of new record `record`, or keep them as a patch
    /// applied when that value is read.
    fn add_to_new_x2many(
        &mut self,
        model: ModelId,
        field: FieldId,
        record: RecId,
        lines: &[RecId],
    ) -> Result<()> {
        if !record.is_new() || lines.is_empty() {
            return Ok(());
        }
        match self.cache.get(model, field, record) {
            Some(Cell::Ids(current)) => {
                let mut value = current.to_vec();
                extend_unique(&mut value, lines.iter().copied());
                self.cache.model_mut(model).set(
                    field,
                    Storage::Ids,
                    record,
                    Cell::Ids(value.into()),
                    false,
                )?;
            }
            _ => {
                let patch = self
                    .engine
                    .patches_mut()
                    .entry((model, field, record))
                    .or_default();
                extend_unique(patch, lines.iter().copied());
            }
        }
        Ok(())
    }

    /// Remove `lines` from the cached x2many value of new record `record` (and from its patch).
    fn remove_from_new_x2many(
        &mut self,
        model: ModelId,
        field: FieldId,
        record: RecId,
        lines: &[RecId],
    ) -> Result<()> {
        if !record.is_new() {
            return Ok(());
        }
        if let Some(patch) = self.engine.patches_mut().get_mut(&(model, field, record)) {
            patch.retain(|p| !lines.contains(p));
        }
        if let Some(Cell::Ids(current)) = self.cache.get(model, field, record)
            && current.iter().any(|c| lines.contains(c))
        {
            let value: Vec<RecId> = current
                .iter()
                .copied()
                .filter(|c| !lines.contains(c))
                .collect();
            self.cache.model_mut(model).set(
                field,
                Storage::Ids,
                record,
                Cell::Ids(value.into()),
                false,
            )?;
        }
        Ok(())
    }

    /// The new lines of a one2many now point at `owner` (or at nothing): their inverse many2one
    /// in the cache, the old owners' cached values without them, the dependents notified.
    async fn set_new_inverse(
        &mut self,
        model: ModelId,
        o2m: FieldId,
        lines: &[RecId],
        owner: Option<RecId>,
    ) -> Result<()> {
        let registry = Arc::clone(self.registry());
        let fs = registry.model_by_id(model).field_by_id(o2m);
        let FieldType::One2many { inverse, .. } = &fs.ty else {
            return Ok(());
        };
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let Some(inv) = comodel.field(inverse) else {
            return Ok(());
        };
        let lines: Vec<RecId> = lines.iter().copied().filter(|l| l.is_new()).collect();
        if lines.is_empty() {
            return Ok(());
        }
        let cell = owner.map_or(Cell::Null, |o| Cell::Id(o.get()));
        let mut changed: Vec<RecId> = Vec::new();
        for line in &lines {
            if let Some(old) = self
                .cache
                .get(comodel.id, inv.id, *line)
                .as_ref()
                .and_then(crate::relational::id_of)
                && Some(old) != owner
            {
                self.remove_from_new_x2many(model, o2m, old, &[*line])?;
            }
            if self
                .cache
                .get(comodel.id, inv.id, *line)
                .is_some_and(|c| c.same_value(&cell))
            {
                continue;
            }
            self.cache.model_mut(comodel.id).set(
                inv.id,
                inv.storage(),
                *line,
                cell.clone(),
                false,
            )?;
            changed.push(*line);
        }
        if let Some(o) = owner {
            self.add_to_new_x2many(model, o2m, o, &lines)?;
        }
        self.modified(comodel.id, vec![(inv.id, changed)]).await
    }

    /// Commands on x2many `field` of new records (REL §4.8.6): the cache only.
    pub(crate) async fn write_x2many_new(
        &mut self,
        model: ModelId,
        field: FieldId,
        groups: Vec<(Records, Vec<Command>)>,
    ) -> Result<()> {
        let Some((first, _)) = groups.first() else {
            return Ok(());
        };
        let env = first.env().clone();
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let co = Records::empty(self.command_env(&env, model, field, comodel), comodel.id);
        let mut all: Vec<RecId> = Vec::new();
        for (recs, _) in &groups {
            for id in recs.rec_ids() {
                push_unique(&mut all, *id);
            }
        }
        let all_records = Records::from_parts(
            env.clone(),
            model,
            Arc::from(all.clone()),
            Arc::from(all.clone()),
        );
        self.ensure_x2many(&all_records, field).await?;
        let mut value: FxHashMap<RecId, Vec<RecId>> = all
            .iter()
            .map(|r| {
                let v = self
                    .cache
                    .get(model, field, *r)
                    .map(|c| ids_in(&c))
                    .unwrap_or_default();
                (*r, v)
            })
            .collect();
        let one2many = matches!(fs.ty, FieldType::One2many { .. }) && fs.store;
        // one2many: every line of the records points back at its record
        if one2many {
            for r in &all {
                let lines = value.get(r).cloned().unwrap_or_default();
                self.set_new_inverse(model, field, &lines, Some(*r)).await?;
            }
        }
        // many2many lines whose mirror must list (or stop listing) a record
        let mut mirror_add: Vec<(RecId, RecId)> = Vec::new();
        let mut mirror_remove: Vec<(RecId, RecId)> = Vec::new();
        for (recs, commands) in &groups {
            let ids: Vec<RecId> = recs.rec_ids().to_vec();
            let Some(&last) = ids.last() else { continue };
            for command in commands {
                match command {
                    Command::Create(vals) => {
                        if matches!(fs.ty, FieldType::Many2many { .. }) {
                            let line = Box::pin(self.new_record(&co, vals.clone(), None, None))
                                .await?
                                .rec_ids()[0];
                            for r in &ids {
                                push_unique(value.entry(*r).or_default(), line);
                                mirror_add.push((*r, line));
                            }
                        } else {
                            for r in &ids {
                                let line = Box::pin(self.new_record(&co, vals.clone(), None, None))
                                    .await?
                                    .rec_ids()[0];
                                push_unique(value.entry(*r).or_default(), line);
                                if one2many {
                                    self.set_new_inverse(model, field, &[line], Some(*r))
                                        .await?;
                                }
                            }
                        }
                    }
                    Command::Update(id, vals) => {
                        let line = align(*id)?;
                        Box::pin(co.browse_ids([line]).write(self, vals.clone())).await?;
                        if !one2many {
                            for r in &ids {
                                push_unique(value.entry(*r).or_default(), line);
                            }
                        }
                    }
                    Command::Delete(id) | Command::Unlink(id) => {
                        let line = align(*id)?;
                        let scope: Vec<RecId> = if matches!(command, Command::Delete(_)) || one2many
                        {
                            all.clone()
                        } else {
                            ids.clone()
                        };
                        for r in &scope {
                            if let Some(v) = value.get_mut(r) {
                                v.retain(|l| *l != line);
                            }
                            mirror_remove.push((*r, line));
                        }
                        if one2many {
                            self.set_new_inverse(model, field, &[line], None).await?;
                        }
                    }
                    Command::Link(id) => {
                        let line = align(*id)?;
                        if one2many {
                            for r in &all {
                                if let Some(v) = value.get_mut(r) {
                                    v.retain(|l| *l != line);
                                }
                            }
                            push_unique(value.entry(last).or_default(), line);
                            self.set_new_inverse(model, field, &[line], Some(last))
                                .await?;
                        } else {
                            for r in &ids {
                                push_unique(value.entry(*r).or_default(), line);
                                mirror_add.push((*r, line));
                            }
                        }
                    }
                    Command::Clear => {
                        for r in &ids {
                            if let Some(v) = value.insert(*r, Vec::new()) {
                                for l in v {
                                    mirror_remove.push((*r, l));
                                }
                            }
                        }
                    }
                    Command::Set(targets) => {
                        let lines = targets
                            .iter()
                            .copied()
                            .map(align)
                            .collect::<Result<Vec<_>>>()?;
                        if one2many {
                            for r in &ids {
                                value.insert(*r, Vec::new());
                            }
                            value.insert(last, lines.clone());
                            self.set_new_inverse(model, field, &lines, Some(last))
                                .await?;
                        } else {
                            for r in &ids {
                                if let Some(old) = value.insert(*r, lines.clone()) {
                                    for l in old {
                                        if !lines.contains(&l) {
                                            mirror_remove.push((*r, l));
                                        }
                                    }
                                }
                                for l in &lines {
                                    mirror_add.push((*r, *l));
                                }
                            }
                        }
                    }
                }
            }
        }
        let storage = fs.storage();
        for r in &all {
            let v = value.remove(r).unwrap_or_default();
            self.cache
                .model_mut(model)
                .set(field, storage, *r, Cell::Ids(v.into()), false)?;
        }
        // the mirror many2many of new lines (never a database record)
        let mirrors: Vec<(ModelId, FieldId)> = self
            .shared
            .relations
            .inverses(model, field)
            .iter()
            .copied()
            .filter(|(m, _)| *m == comodel.id)
            .collect();
        if matches!(fs.ty, FieldType::Many2many { .. }) {
            for (m, f) in &mirrors {
                for (r, line) in &mirror_remove {
                    self.remove_from_new_x2many(*m, *f, *line, &[*r])?;
                }
                for (r, line) in &mirror_add {
                    self.add_to_new_x2many(*m, *f, *line, &[*r])?;
                }
            }
        }
        self.modified(model, vec![(field, all)]).await
    }

    /// Commands on a non-stored x2many (REL §4.8.8): applied to the current value in the cache,
    /// whether the records are new or not; database lines named by `UPDATE` are written.
    pub(crate) async fn write_x2many_cache(
        &mut self,
        model: ModelId,
        field: FieldId,
        groups: Vec<(Records, Vec<Command>)>,
    ) -> Result<()> {
        let Some((first, _)) = groups.first() else {
            return Ok(());
        };
        let env = first.env().clone();
        let registry = Arc::clone(env.registry());
        let fs = registry.model_by_id(model).field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let co = Records::empty(self.command_env(&env, model, field, comodel), comodel.id);
        let mut value: FxHashMap<RecId, Vec<RecId>> = FxHashMap::default();
        for (recs, _) in &groups {
            let cells = self.field_cells(recs, field).await?;
            for (r, cell) in recs.rec_ids().iter().zip(cells) {
                value.entry(*r).or_insert_with(|| ids_in(&cell));
            }
        }
        let mut all: Vec<RecId> = Vec::new();
        for (recs, commands) in &groups {
            let ids: Vec<RecId> = recs.rec_ids().to_vec();
            for r in &ids {
                push_unique(&mut all, *r);
            }
            let Some(&last) = ids.last() else { continue };
            for command in commands {
                match command {
                    Command::Set(targets) => {
                        for r in &ids {
                            value.insert(*r, Vec::new());
                        }
                        value.insert(last, targets.clone());
                    }
                    Command::Clear => {
                        for r in &ids {
                            value.insert(*r, Vec::new());
                        }
                    }
                    Command::Link(id) => push_unique(value.entry(last).or_default(), *id),
                    Command::Create(vals) => {
                        for r in &ids {
                            let line = Box::pin(self.new_record(&co, vals.clone(), None, None))
                                .await?
                                .rec_ids()[0];
                            push_unique(value.entry(*r).or_default(), line);
                        }
                    }
                    Command::Update(id, vals) => {
                        Box::pin(co.browse_ids([*id]).write(self, vals.clone())).await?;
                    }
                    Command::Delete(id) | Command::Unlink(id) => {
                        for v in value.values_mut() {
                            v.retain(|l| l != id);
                        }
                    }
                }
            }
        }
        let slot = self.slot(&env, model, field)?;
        let cells: Vec<(RecId, Cell)> = all
            .iter()
            .map(|r| (*r, Cell::Ids(value.remove(r).unwrap_or_default().into())))
            .collect();
        self.slot_set(&slot, model, field, cells)?;
        self.modified(model, vec![(field, all)]).await
    }

    pub(crate) async fn inverse_x2many_cache(
        &mut self,
        records: &Records,
        field: FieldId,
    ) -> Result<()> {
        let spec = records.spec();
        let fs = spec.field_by_id(field);
        let Some(method) = fs.inverse.filter(|_| !fs.store) else {
            return Ok(());
        };
        let real: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|i| !i.is_new())
            .collect();
        if real.is_empty() {
            return Ok(());
        }
        let model = records.model_id();
        let group: Vec<FieldId> = self
            .shared
            .graph
            .model(model)
            .compute(field)
            .map_or_else(|| vec![field], |fc| fc.group.to_vec());
        let real = records.browse_ids(real);
        let guard = self.guard();
        let depth = self.protect(model, &group, real.rec_ids());
        let result = crate::dispatch::call_method::<(), ()>(self, &real, method, ()).await;
        self.unprotect_to(depth);
        guard.disarm();
        result?;
        self.cache
            .model_mut(model)
            .invalidate(Some(&group), Some(real.rec_ids()));
        self.engine
            .context_mut()
            .invalidate(model, Some(&group), Some(real.rec_ids()));
        let names: FxHashSet<FieldId> = [field].into_iter().collect();
        self.run_constraints(&real, &names, &FxHashSet::default())
            .await
    }

    // ---- back to commands (convert_to_write) ----------------------------------------------------

    /// Commands that reproduce `ids` (an x2many value of `field`) on database records:
    /// `SET` of the database records and origins, then `CREATE`/`UPDATE` per new line in order
    /// (REL §4.8.7). Database ids pass through unchanged.
    pub(crate) async fn commands_for_ids(
        &mut self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<Vec<Command>> {
        let registry = Arc::clone(env.registry());
        let fs = registry.model_by_id(model).field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let skip: Vec<FieldId> = self
            .shared
            .relations
            .inverses(model, field)
            .iter()
            .filter(|(m, _)| *m == comodel.id)
            .map(|(_, f)| *f)
            .collect();
        let mut set: Vec<RecId> = Vec::new();
        let mut rest: Vec<Command> = Vec::new();
        for id in ids {
            match (id.as_real(), id.origin()) {
                (Some(_), _) => set.push(*id),
                (None, Some(origin)) => {
                    let origin = RecId::real(origin);
                    set.push(origin);
                    let changed =
                        Box::pin(self.changed_vals(env, comodel, *id, origin, &skip)).await?;
                    if !changed.is_empty() {
                        rest.push(Command::Update(origin, changed));
                    }
                }
                (None, None) => {
                    let vals = Box::pin(self.new_vals(env, comodel, *id, &skip)).await?;
                    rest.push(Command::Create(vals));
                }
            }
        }
        let mut out = vec![Command::Set(set)];
        out.extend(rest);
        Ok(out)
    }

    /// The cached values of new record `id` in write format (REL §4.8.7): writable fields only,
    /// many2one values to new records mapped to their origins, x2many values as commands.
    async fn new_vals(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        id: RecId,
        skip: &[FieldId],
    ) -> Result<RawVals> {
        let mut vals = RawVals::new();
        self.prepare_draft_properties(env, spec, id).await?;
        for field in &spec.fields {
            if !writable(field, skip) {
                continue;
            }
            let Some(cell) = self.draft_cell(env, spec, field, id)? else {
                continue;
            };
            if field.ty.is_x2many() {
                let ids = self.visible_ids_of(env, spec.id, id, field.id).await?;
                let commands = self.commands_for_ids(env, spec.id, field.id, &ids).await?;
                vals.insert_commands(field.name.into(), commands);
                continue;
            }
            vals.insert(field.name.into(), write_cell(spec, field, cell)?);
        }
        Ok(vals)
    }

    /// The ids of x2many `field` of new record `id` that `env` may see (its rules and the active
    /// test); the cache holds the origin's raw value.
    async fn visible_ids_of(
        &mut self,
        env: &Env,
        model: ModelId,
        id: RecId,
        field: FieldId,
    ) -> Result<Vec<RecId>> {
        let one = Records::from_parts(env.clone(), model, Arc::from([id]), Arc::from([id]));
        let cells = self.x2many_cells(&one, field).await?;
        let cell = cells.first().ok_or_else(|| {
            Error::programming("x2many_cells returned no cell for a single record")
        })?;
        Ok(ids_in(cell))
    }

    /// The cached values of new record `id` that differ from its origin's, in write format.
    async fn changed_vals(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        id: RecId,
        origin: RecId,
        skip: &[FieldId],
    ) -> Result<RawVals> {
        let mut vals = RawVals::new();
        let origin_records = Records::from_parts(
            env.engine_sudo(),
            spec.id,
            Arc::from([origin]),
            Arc::from([origin]),
        );
        let reader_origin = Records::from_parts(
            env.clone(),
            spec.id,
            Arc::from([origin]),
            Arc::from([origin]),
        );
        self.prepare_draft_properties(env, spec, id).await?;
        for field in &spec.fields {
            if !writable(field, skip) {
                continue;
            }
            let Some(cell) = self.draft_cell(env, spec, field, id)? else {
                continue;
            };
            if field.ty.is_x2many() {
                let visible = self.visible_ids_of(env, spec.id, id, field.id).await?;
                let mine: Vec<RecId> = visible
                    .iter()
                    .map(|i| i.as_real().or(i.origin()).map_or(*i, RecId::real))
                    .collect();
                let current = self
                    .x2many_cells(&reader_origin, field.id)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        Error::programming("x2many_cells returned no cell for a single record")
                    })?;
                if mine != ids_in(&current) {
                    let commands = self
                        .commands_for_ids(env, spec.id, field.id, &visible)
                        .await?;
                    vals.insert_commands(field.name.into(), commands);
                }
                continue;
            }
            let current = self
                .field_cells(&origin_records, field.id)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::programming("field_cells returned no cell for a single record")
                })?;
            let cell = write_cell(spec, field, cell)?;
            if !cell.same_value(&current) {
                vals.insert(field.name.into(), cell);
            }
        }
        Ok(vals)
    }

    /// The company fallbacks a projection of draft `id` needs, prepared once for all its
    /// company-dependent fields, and only when one of them is cached (an uncached field is not
    /// written, so a draft without company values needs no current company).
    async fn prepare_draft_properties(
        &mut self,
        env: &Env,
        spec: &ModelSpec,
        id: RecId,
    ) -> Result<()> {
        if spec
            .fields
            .iter()
            .any(|f| f.company_dependent && self.cache.contains(spec.id, f.id, id))
        {
            self.prepare_properties(env, spec).await?;
        }
        Ok(())
    }

    /// The cached value of draft `id` in write format's input: a company map is projected to the
    /// environment's company, the only company a create or write stores.
    fn draft_cell(
        &self,
        env: &Env,
        spec: &ModelSpec,
        field: &FieldSpec,
        id: RecId,
    ) -> Result<Option<Cell>> {
        if !self.cache.contains(spec.id, field.id, id) {
            return Ok(None);
        }
        if field.company_dependent {
            return Ok(self
                .cells_of(env, spec.id, field.id, &[id])?
                .into_iter()
                .next());
        }
        Ok(self.cache.get(spec.id, field.id, id))
    }
}

pub(crate) fn inherited_on_new(spec: &erp_registry::ModelSpec, field: &str) -> Error {
    Error::programming(format!(
        "{}.{field} is a field of an _inherits parent: values of delegated fields on new records \
         are not supported yet (a new parent record would be needed; S4b follow-up, not \
         scheduled), and would otherwise be lost",
        spec.name
    ))
}

/// Delegated value `field` written on new record `id`, whose delegate `delegate` does not point
/// at an in-memory parent: it is empty (`link` is `None`) or a database record of `parent`.
pub(crate) fn delegated_on_new_without_parent(
    spec: &erp_registry::ModelSpec,
    field: &str,
    id: RecId,
    delegate: &str,
    parent: &str,
    link: Option<RecId>,
) -> Error {
    let state = match link {
        None => format!("has no {delegate}"),
        Some(p) => format!("has {delegate} = {parent}({p},), a database record"),
    };
    let effect = match link {
        None => "the value would be lost",
        Some(_) => "the database record would change",
    };
    Error::programming(format!(
        "{}.{field}: new record {id} {state}; delegated values of new records go to an in-memory \
         parent, so {effect}",
        spec.name
    ))
}

/// A cached value in write format: a many2one to a new record becomes its origin; a new record
/// without origin cannot be written (it has no database id).
fn write_cell(spec: &ModelSpec, field: &FieldSpec, cell: Cell) -> Result<Cell> {
    match (&field.ty, &cell) {
        (FieldType::Many2one { .. }, Cell::Id(raw)) if *raw < 0 => {
            let id = RecId::from_raw(*raw)
                .and_then(RecId::origin)
                .ok_or_else(|| {
                    Error::programming(format!(
                        "{}.{} points at a new record without origin, which cannot be written",
                        spec.name, field.name
                    ))
                })?;
            Ok(Cell::Id(id.get()))
        }
        _ => Ok(cell),
    }
}

/// A value given to `new()`: validated like a write, except that a many2one may point at a new
/// record: the new record of a database origin, or an active originless cache identity of the comodel
/// in this transaction. Rolled-back and failed-construction identities are refused.
pub(crate) fn new_record_cell(
    spec: &ModelSpec,
    field: &FieldSpec,
    cell: Cell,
    tx: &Transaction,
) -> Result<Cell> {
    if let (FieldType::Many2one { comodel, .. }, Cell::Id(raw)) = (&field.ty, &cell)
        && *raw < 0
    {
        let known = RecId::from_raw(*raw).is_some_and(|id| {
            id.origin().is_some()
                || tx.engine.new_records.get(&id).is_some_and(|model| {
                    tx.shared.registry.model_by_id(*model).name == comodel.as_str()
                })
        });
        if !known {
            return Err(Error::validation(format!(
                "Wrong value for {}.{}: {raw} is not a new record of this transaction",
                spec.name, field.name
            )));
        }
        return Ok(cell);
    }
    erp_fields::convert::validate_cell(
        erp_fields::convert::FieldRef {
            model: spec.name,
            field: field.name,
            label: &field.string,
        },
        &field.ty,
        cell,
    )
}

impl Records {
    /// `convert_to_write` of an x2many field of one record (REL §4.8.7): the
    /// commands that reproduce its current value on a database record — `SET` of the database
    /// lines (origins of new lines included), then `CREATE` for each new line without origin and
    /// `UPDATE` with the changed values for each new line that differs from its origin.
    pub async fn x2many_commands(&self, tx: &mut Transaction, field: &str) -> Result<Vec<Command>> {
        let spec = self.spec();
        let fs = spec.field(field).ok_or_else(|| {
            Error::programming(format!("Invalid field '{field}' on '{}'", spec.name))
        })?;
        if !fs.ty.is_x2many() {
            return Err(Error::programming(format!(
                "{}.{field} is not a one2many or many2many field",
                spec.name
            )));
        }
        let cell = self.cell(tx, field).await?;
        let ids = ids_in(&cell);
        tx.commands_for_ids(self.env(), self.model_id(), fs.id, &ids)
            .await
    }
}
