//! x2many commands (REL §4.8): the command protocol shared by Rust code, data files and RPC, and
//! how one2many and many2many fields apply it in write mode and in create mode.
//!
//! - **One2many** (REL §4.8.3): `UPDATE`, `CREATE`, `DELETE`, `UNLINK` and `LINK` are deferred
//!   to *flush points* — before a `CLEAR`/`SET` and at the end — where updates run first (one
//!   `write` per run of equal values), then detaching (one `write`), deletes, one `create()` for
//!   every collected line, and links (one `write` per record). `UNLINK` deletes the line when the
//!   inverse many2one cascades, else empties the inverse.
//!   Over a
//!   many2one_reference inverse, the model field is written with the inverse (F2). Once a `CREATE` or `LINK` was
//!   seen (and always in create mode) `CLEAR` is ignored and `SET` only links (the full-delete
//!   guard). `LINK` and `SET` attach lines to the *last* record written.
//! - **Many2many** (REL §4.8.4, §4.7.6): commands edit an in-memory copy of the relation; then
//!   comodel records to create are created in **one** `create()` for every record and every
//!   command list (batched across parent records), records to delete are deleted,
//!   and the difference with the old relation is written immediately with at most one `INSERT …
//!   ON CONFLICT DO NOTHING` and one `DELETE` of pairs (nothing at all when the relation did not
//!   change). Archived targets are kept. Sibling and inverse
//!   many2many values in cache are patched in place (/036).

use std::borrow::Cow;
use std::sync::Arc;

use erp_domain::{Domain, Operator, Value as DValue};
use erp_fields::{Cell, Error, FieldType, OnDelete, RecId, Result, Value};
use erp_registry::{FieldId, ModelId};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::records::Records;
use crate::vals::RawVals;

/// ```
/// use erp_orm::Command;
/// use erp_fields::RecId;
///
/// let cmd = Command::link(RecId::from_real(42).unwrap());
/// assert!(matches!(cmd, Command::Link(_)));
///
/// let clear = Command::clear();
/// assert_eq!(clear, Command::Clear);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `(0, 0, values)`: create a comodel record and link it.
    Create(RawVals),
    /// `(1, id, values)`: write `values` on the record (it is not linked).
    Update(RecId, RawVals),
    /// `(2, id, 0)`: delete the record (and its links).
    Delete(RecId),
    /// `(3, id, 0)`: remove the link.
    Unlink(RecId),
    /// `(4, id, 0)`: add a link.
    Link(RecId),
    /// `(5, 0, 0)`: remove every link.
    Clear,
    /// `(6, 0, ids)`: replace the links by `ids`.
    Set(Vec<RecId>),
}

impl Command {
    /// `CREATE` with values for the comodel (`Vals<C>` or `RawVals`).
    pub fn create(vals: impl Into<RawVals>) -> Self {
        Self::Create(vals.into())
    }

    /// `UPDATE`.
    pub fn update(id: RecId, vals: impl Into<RawVals>) -> Self {
        Self::Update(id, vals.into())
    }

    /// `DELETE`.
    pub fn delete(id: RecId) -> Self {
        Self::Delete(id)
    }

    /// `UNLINK`.
    pub fn unlink(id: RecId) -> Self {
        Self::Unlink(id)
    }

    /// `LINK`.
    pub fn link(id: RecId) -> Self {
        Self::Link(id)
    }

    /// `CLEAR`.
    pub fn clear() -> Self {
        Self::Clear
    }

    /// `SET`.
    pub fn set(ids: impl IntoIterator<Item = RecId>) -> Self {
        Self::Set(ids.into_iter().collect())
    }

    pub fn code(&self) -> i64 {
        match self {
            Self::Create(_) => 0,
            Self::Update(..) => 1,
            Self::Delete(_) => 2,
            Self::Unlink(_) => 3,
            Self::Link(_) => 4,
            Self::Clear => 5,
            Self::Set(_) => 6,
        }
    }

    pub fn to_value(&self) -> Value {
        let vals = |v: &RawVals| {
            let mut map: erp_fields::ValueMap = v
                .iter()
                .map(|(k, c)| (k.to_owned(), Value::from_cell(c)))
                .collect();
            for (k, cmds) in v.iter_commands() {
                map.insert(
                    k.to_owned(),
                    Value::list(cmds.iter().map(Command::to_value)),
                );
            }
            Value::Map(Arc::new(map))
        };
        let code = Value::Int(self.code());
        match self {
            Self::Create(v) => Value::list([code, Value::Int(0), vals(v)]),
            Self::Update(id, v) => Value::list([code, Value::Int(id.get()), vals(v)]),
            Self::Delete(id) | Self::Unlink(id) | Self::Link(id) => {
                Value::list([code, Value::Int(id.get()), Value::Int(0)])
            }
            Self::Clear => Value::list([code, Value::Int(0), Value::Int(0)]),
            Self::Set(ids) => Value::list([
                code,
                Value::Int(0),
                Value::list(ids.iter().map(|i| Value::Int(i.get()))),
            ]),
        }
    }
}

fn wrong_value(records: &Records, field: &str, value: &Value) -> Error {
    Error::validation(format!(
        "Wrong value for {}.{field}: {value}",
        records.model_name()
    ))
}

fn positive_id(records: &Records, field: &str, value: &Value) -> Result<RecId> {
    match value {
        Value::Int(i) => RecId::from_real(*i).ok_or_else(|| wrong_value(records, field, value)),
        other => Err(wrong_value(records, field, other)),
    }
}

impl Records {
    /// x2many write values from the dynamic surface (REL §4.8.2): a list of command triples, a
    /// list of ids (`SET`), `False` (`CLEAR`), `[]` (nothing). Values of `CREATE`/`UPDATE` are
    /// converted with the comodel's fields (`comodel` is a recordset of it). Anything else, and
    /// unknown command codes, are `Validation` errors naming the field.
    pub fn commands_from_value(
        &self,
        comodel: &Records,
        field: &str,
        value: &Value,
    ) -> Result<Vec<Command>> {
        let items = match value {
            Value::False | Value::Bool(false) => return Ok(vec![Command::Clear]),
            Value::List(items) => items,
            other => return Err(wrong_value(self, field, other)),
        };
        if items.is_empty() {
            return Ok(Vec::new());
        }
        match &items[0] {
            Value::List(_) => {}
            Value::Map(_) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    match item {
                        Value::Map(map) => {
                            out.push(Command::Create(comodel.vals_from_values(map.iter())?));
                        }
                        other => return Err(wrong_value(self, field, other)),
                    }
                }
                return Ok(out);
            }
            _ => {
                let ids = items
                    .iter()
                    .map(|v| positive_id(self, field, v))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(vec![Command::Set(ids)]);
            }
        }
        let map_vals = |v: Option<&Value>| -> Result<RawVals> {
            match v {
                Some(Value::Map(map)) => comodel.vals_from_values(map.iter()),
                Some(Value::False) | Some(Value::Int(0)) | None => Ok(RawVals::new()),
                Some(other) => Err(wrong_value(self, field, other)),
            }
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items.iter() {
            // exactly three elements (F5)
            let Value::List(triple) = item else {
                return Err(wrong_value(self, field, item));
            };
            if triple.len() != 3 {
                return Err(wrong_value(self, field, item));
            }
            let code = match triple.first() {
                Some(Value::Int(c)) => *c,
                _ => return Err(wrong_value(self, field, item)),
            };
            let id = || -> Result<RecId> {
                positive_id(self, field, triple.get(1).unwrap_or(&Value::False))
            };
            out.push(match code {
                0 => Command::Create(map_vals(triple.get(2))?),
                1 => Command::Update(id()?, map_vals(triple.get(2))?),
                2 => Command::Delete(id()?),
                3 => Command::Unlink(id()?),
                4 => Command::Link(id()?),
                5 => Command::Clear,
                6 => {
                    let ids = match triple.get(2) {
                        // `SET` takes a list of ids (F5)
                        Some(Value::List(ids)) => ids
                            .iter()
                            .map(|v| positive_id(self, field, v))
                            .collect::<Result<Vec<_>>>()?,
                        _ => return Err(wrong_value(self, field, item)),
                    };
                    Command::Set(ids)
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unknown command {other} for {}.{field}: {item}",
                        self.model_name()
                    )));
                }
            });
        }
        Ok(out)
    }
}

/// The old and new id sets of a record in a tracked many2many write. The write computes both for
/// every record it touches, so a missing one is a bug: fail loudly, never skip the record
/// (r1, M1).
fn tracked_sets<'a>(
    model: &str,
    field: &str,
    id: RecId,
    old: Option<&'a OrderedIds>,
    new: Option<&'a OrderedIds>,
) -> Result<(&'a OrderedIds, &'a OrderedIds)> {
    match (old, new) {
        (Some(o), Some(n)) => Ok((o, n)),
        _ => Err(Error::programming(format!(
            "tracked many2many {model}.{field}: record {id:?} has no {} id set; its change \
             cannot be captured",
            if old.is_none() { "old" } else { "new" }
        ))),
    }
}

/// An insertion-ordered set of ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OrderedIds(Vec<RecId>);

impl OrderedIds {
    fn add(&mut self, id: RecId) {
        if !self.0.contains(&id) {
            self.0.push(id);
        }
    }

    fn remove(&mut self, id: RecId) {
        self.0.retain(|i| *i != id);
    }

    fn contains(&self, id: RecId) -> bool {
        self.0.contains(&id)
    }
}

/// Collect `(record, line)` pairs keyed by record in first-seen order.
#[derive(Debug, Default)]
struct Links(Vec<(RecId, Vec<RecId>)>);

impl Links {
    fn add(&mut self, record: RecId, lines: impl IntoIterator<Item = RecId>) {
        let entry = match self.0.iter_mut().position(|(r, _)| *r == record) {
            Some(i) => &mut self.0[i].1,
            None => {
                self.0.push((record, Vec::new()));
                let last = self.0.len() - 1;
                &mut self.0[last].1
            }
        };
        for line in lines {
            if !entry.contains(&line) {
                entry.push(line);
            }
        }
    }
}

/// One2many state of one write (deferred work, REL §4.8.3).
struct O2mWrite {
    comodel: Records,
    inverse: &'static str,
    /// For a one2many over a many2one_reference: the comodel's model field and the owner's model
    /// name, written with the inverse (F2).
    model_field: Option<(&'static str, &'static str)>,
    cascade: bool,
    /// `UPDATE` values, consecutive equal values merged (one `write` per group).
    to_update: Vec<(RawVals, Vec<RecId>)>,
    to_create: Vec<RawVals>,
    to_delete: Vec<RecId>,
    /// Lines whose inverse is emptied (`UNLINK` without cascade), in one `write`.
    to_detach: Vec<RecId>,
    to_link: Links,
    /// Owner of each line named by `UPDATE`/`DELETE`/`UNLINK`, read once and kept up to date by
    /// the commands of the list (F3).
    owners: FxHashMap<RecId, Option<RecId>>,
}

impl O2mWrite {
    /// The values attaching lines to `record`.
    fn link_vals(&self, record: RecId) -> RawVals {
        let vals = RawVals::new().set_cell(self.inverse, Cell::Id(record.get()));
        match self.model_field {
            Some((field, model)) => vals.set_cell(field, Cell::Str(model.into())),
            None => vals,
        }
    }

    fn add_update(&mut self, id: RecId, vals: &RawVals) {
        match self.to_update.last_mut() {
            Some((last, ids)) if last == vals => {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            _ => self.to_update.push((vals.clone(), vec![id])),
        }
    }
}

impl Transaction {
    /// Apply x2many commands: per group, the records and their command list (create mode: one
    /// group per created record).
    pub(crate) async fn write_x2many(
        &mut self,
        model: ModelId,
        field: FieldId,
        groups: Vec<(Records, Vec<Command>)>,
        create: bool,
    ) -> Result<()> {
        let groups: Vec<(Records, Vec<Command>)> = groups
            .into_iter()
            .filter(|(r, c)| !r.is_empty() && !c.is_empty())
            .collect();
        let Some((first, _)) = groups.first() else {
            return Ok(());
        };
        let registry = Arc::clone(first.env().registry());
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let any_new = groups
            .iter()
            .any(|(r, _)| r.rec_ids().iter().any(|i| i.is_new()));
        if any_new
            && groups
                .iter()
                .any(|(r, _)| r.rec_ids().iter().any(|i| !i.is_new()))
        {
            return Err(Error::programming(format!(
                "{} contains a mix of real and new records. It is not supported.",
                first
            )));
        }
        if !fs.ty.is_x2many() {
            return Err(Error::programming(format!(
                "{}.{} is not an x2many field",
                spec.name, fs.name
            )));
        }
        if !fs.store {
            // the current (computed) value in the cache (REL §4.8.8)
            return self.write_x2many_cache(model, field, groups).await;
        }
        if any_new {
            // new records: the cache only (REL §4.8.6)
            return self.write_x2many_new(model, field, groups).await;
        }
        // new records named by LINK/SET on database records are created (or, with an origin,
        // linked and updated) as `convert_to_write` does
        let mut realized: Vec<(Records, Vec<Command>)> = Vec::with_capacity(groups.len());
        for (recs, commands) in groups {
            let commands = self.realize_commands(&recs, model, field, commands).await?;
            realized.push((recs, commands));
        }
        let groups = realized;
        match &fs.ty {
            FieldType::One2many { .. } => self.write_o2m(model, field, groups, create).await,
            FieldType::Many2many { .. } => self.write_m2m(model, field, groups).await,
            _ => Err(Error::programming(format!(
                "{}.{} is not an x2many field",
                spec.name, fs.name
            ))),
        }
    }

    /// Commands for database records with the new records they name replaced (REL §4.8.7): a new
    /// record with an origin is its origin (plus an `UPDATE` of what changed), one without origin
    /// is created.
    async fn realize_commands(
        &mut self,
        records: &Records,
        model: ModelId,
        field: FieldId,
        commands: Vec<Command>,
    ) -> Result<Vec<Command>> {
        if !commands.iter().any(|c| match c {
            Command::Link(id) | Command::Update(id, _) => id.is_new(),
            Command::Set(ids) => ids.iter().any(|i| i.is_new()),
            _ => false,
        }) {
            return Ok(commands);
        }
        let env = records.env().clone();
        let mut out: Vec<Command> = Vec::with_capacity(commands.len());
        for command in commands {
            match command {
                Command::Set(ids) if ids.iter().any(|i| i.is_new()) => {
                    out.extend(self.commands_for_ids(&env, model, field, &ids).await?);
                }
                Command::Link(id) if id.is_new() => {
                    for c in self.commands_for_ids(&env, model, field, &[id]).await? {
                        match c {
                            Command::Set(ids) => out.extend(ids.into_iter().map(Command::Link)),
                            other => out.push(other),
                        }
                    }
                }
                Command::Update(id, vals) if id.is_new() => match id.origin() {
                    Some(origin) => out.push(Command::Update(RecId::real(origin), vals)),
                    None => {
                        return Err(Error::programming(format!(
                            "UPDATE of the new record {id} on database records: create it first"
                        )));
                    }
                },
                other => out.push(other),
            }
        }
        Ok(out)
    }

    // ----- one2many ----------------------------------------------------------------------------

    async fn write_o2m(
        &mut self,
        model: ModelId,
        field: FieldId,
        groups: Vec<(Records, Vec<Command>)>,
        create: bool,
    ) -> Result<()> {
        let env = groups[0].0.env().clone();
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let FieldType::One2many { inverse, .. } = &fs.ty else {
            return Ok(());
        };
        let inverse_spec = comodel.field(inverse).ok_or_else(|| {
            Error::programming(format!(
                "'{inverse}' declared in {}.{} does not exist on '{comodel}'.",
                spec.name,
                fs.name,
                comodel = comodel.name
            ))
        })?;
        let co_env = self.command_env(&env, model, field, comodel);
        let model_field = self
            .shared
            .relations
            .model_field(model, field)
            .map(|mf| (mf, spec.name));
        let mut state = O2mWrite {
            comodel: Records::empty(co_env, comodel.id),
            inverse: inverse_spec.name,
            model_field,
            cascade: matches!(
                inverse_spec.ty,
                FieldType::Many2one {
                    ondelete: OnDelete::Cascade,
                    ..
                }
            ),
            to_update: Vec::new(),
            to_create: Vec::new(),
            to_delete: Vec::new(),
            to_detach: Vec::new(),
            to_link: Links::default(),
            owners: FxHashMap::default(),
        };
        // the lines named by UPDATE/DELETE/UNLINK must be lines of the written records: their
        // owners are read once (F3)
        let mut seen = FxHashSet::default();
        let named: Vec<RecId> = groups
            .iter()
            .flat_map(|(_, commands)| commands.iter())
            .filter_map(|c| match c {
                Command::Update(id, _) | Command::Delete(id) | Command::Unlink(id) => Some(*id),
                _ => None,
            })
            .filter(|id| seen.insert(*id))
            .collect();
        if !named.is_empty() {
            state.owners = self.o2m_owners(&state, &named).await?;
        }
        let field_domain = self.shared.relations.domain(model, field);
        let declared_domain = self.shared.relations.declared_domain(model, field);
        let mut allow_full_delete = !create;
        for (recs, commands) in &groups {
            let Some(&last) = recs.rec_ids().last() else {
                continue;
            };
            let member = |owners: &FxHashMap<RecId, Option<RecId>>, id: RecId| -> Result<()> {
                match owners.get(&id).copied().flatten() {
                    Some(owner) if recs.rec_ids().contains(&owner) => Ok(()),
                    _ => Err(Error::validation(format!(
                        "Record {},{} is not a line of {}.{} of {recs}: link it first",
                        comodel.name,
                        id.get(),
                        spec.name,
                        fs.name
                    ))),
                }
            };
            for command in commands {
                match command {
                    Command::Create(vals) => {
                        for r in recs.rec_ids() {
                            let mut v = vals.clone();
                            for (name, cell) in state.link_vals(*r).iter() {
                                v.insert(Cow::Owned(name.to_owned()), cell.clone());
                            }
                            state.to_create.push(v);
                        }
                        allow_full_delete = false;
                    }
                    Command::Update(id, vals) => {
                        member(&state.owners, *id)?;
                        state.add_update(*id, vals);
                    }
                    Command::Delete(id) => {
                        member(&state.owners, *id)?;
                        state.to_delete.push(*id);
                        state.owners.insert(*id, None);
                    }
                    Command::Unlink(id) => {
                        member(&state.owners, *id)?;
                        if state.cascade {
                            state.to_delete.push(*id);
                        } else {
                            state.to_detach.push(*id);
                        }
                        state.owners.insert(*id, None);
                    }
                    Command::Link(id) => {
                        state.to_link.add(last, [*id]);
                        if let Some(owner) = state.owners.get_mut(id) {
                            *owner = Some(last);
                        }
                        allow_full_delete = false;
                    }
                    Command::Clear | Command::Set(_) => {
                        let ids: &[RecId] = match command {
                            Command::Set(ids) => ids,
                            _ => &[],
                        };
                        if !allow_full_delete {
                            // the full-delete guard: only link
                            if !ids.is_empty() {
                                state.to_link.add(last, ids.iter().copied());
                            }
                            continue;
                        }
                        self.o2m_flush(
                            &mut state,
                            model,
                            field,
                            &declared_domain,
                            spec.name,
                            fs.name,
                        )
                        .await?;
                        let rec_ids: Vec<i64> = recs.ids();
                        let keep: Vec<i64> = ids.iter().map(|i| i.get()).collect();
                        let domain = field_domain.clone()
                            & Domain::condition(state.inverse, Operator::In, DValue::list(rec_ids))
                                .map_err(Error::from)?
                            & Domain::condition("id", Operator::NotIn, DValue::list(keep))
                                .map_err(Error::from)?;
                        let found = state.comodel.search(self, &domain).await?;
                        let found: Vec<RecId> = found.rec_ids().to_vec();
                        for id in &found {
                            if let Some(owner) = state.owners.get_mut(id) {
                                *owner = None;
                            }
                        }
                        if state.cascade {
                            state.to_delete.extend_from_slice(&found);
                        } else {
                            state.to_detach.extend_from_slice(&found);
                        }
                        if !ids.is_empty() {
                            self.o2m_flush(
                                &mut state,
                                model,
                                field,
                                &declared_domain,
                                spec.name,
                                fs.name,
                            )
                            .await?;
                            let lines = state.comodel.browse_ids(ids.iter().copied());
                            lines.write(self, state.link_vals(last)).await?;
                            for id in ids {
                                if let Some(owner) = state.owners.get_mut(id) {
                                    *owner = Some(last);
                                }
                            }
                        }
                    }
                }
            }
        }
        self.o2m_flush(
            &mut state,
            model,
            field,
            &declared_domain,
            spec.name,
            fs.name,
        )
        .await
    }

    /// The owner of each line (`None` when it has none, belongs to another model, or does not
    /// exist), read as superuser in one fetch.
    async fn o2m_owners(
        &mut self,
        state: &O2mWrite,
        lines: &[RecId],
    ) -> Result<FxHashMap<RecId, Option<RecId>>> {
        let co = state.comodel.sudo(true).browse_ids(lines.iter().copied());
        let mut names = vec![state.inverse];
        if let Some((mf, _)) = state.model_field {
            names.push(mf);
        }
        co.fetch(self, Some(&names)).await?;
        let spec = co.spec();
        let inverse = spec.field_id(state.inverse);
        let model_field = state.model_field.and_then(|(mf, _)| spec.field_id(mf));
        let model = co.model_id();
        Ok(lines
            .iter()
            .map(|line| {
                let owner = inverse
                    .and_then(|f| self.cache.get(model, f, *line))
                    .as_ref()
                    .and_then(crate::relational::id_of);
                let same_model = match (model_field, state.model_field) {
                    (Some(mf), Some((_, owner_model))) => matches!(
                        self.cache.get(model, mf, *line),
                        Some(Cell::Str(m)) if &*m == owner_model
                    ),
                    _ => true,
                };
                (*line, owner.filter(|_| same_model))
            })
            .collect())
    }

    /// A flush point: updates, detached lines, deletes, then one create for every collected
    /// line, then links (one write per record).
    async fn o2m_flush(
        &mut self,
        state: &mut O2mWrite,
        model: ModelId,
        field: FieldId,
        field_domain: &Domain,
        model_name: &str,
        field_name: &str,
    ) -> Result<()> {
        let inaccessible = || {
            Error::programming(format!(
                "Cannot link inaccessible records in {model_name}.{field_name} ({field_domain})"
            ))
        };
        for (vals, ids) in std::mem::take(&mut state.to_update) {
            state.comodel.browse_ids(ids).write(self, vals).await?;
        }
        if !state.to_detach.is_empty() {
            let lines = state
                .comodel
                .browse_ids(std::mem::take(&mut state.to_detach));
            lines
                .write(self, RawVals::new().set_cell(state.inverse, Cell::Null))
                .await?;
        }
        let mut before: FxHashMap<RecId, FxHashSet<RecId>> = FxHashMap::default();
        if !state.to_link.0.is_empty() {
            let records: Vec<RecId> = state.to_link.0.iter().map(|(r, _)| *r).collect();
            let env = state.comodel.env().clone();
            let owners =
                Records::from_parts(env, model, Arc::from(records.clone()), Arc::from(records));
            let cells = self.x2many_cells(&owners, field).await?;
            for (r, cell) in owners.rec_ids().iter().zip(cells) {
                if let Cell::Ids(ids) = cell {
                    before.insert(*r, ids.iter().copied().collect());
                }
            }
        }
        if !state.to_delete.is_empty() {
            let lines = state
                .comodel
                .browse_ids(std::mem::take(&mut state.to_delete));
            lines.unlink(self).await?;
        }
        if !state.to_create.is_empty() {
            let vals = std::mem::take(&mut state.to_create);
            let lines = state.comodel.create(self, vals).await?;
            if !field_domain.is_true()
                && self.filter_domain(&lines, field_domain).await?.len() < lines.len()
            {
                return Err(inaccessible());
            }
        }
        for (record, line_ids) in std::mem::take(&mut state.to_link.0) {
            let known = before.get(&record);
            let lines: Vec<RecId> = line_ids
                .into_iter()
                .filter(|l| known.is_none_or(|k| !k.contains(l)))
                .collect();
            if lines.is_empty() {
                continue;
            }
            let lines = state.comodel.browse_ids(lines);
            // linking missing lines fails
            lines.cells(self, state.inverse).await?;
            if !field_domain.is_true()
                && self.filter_domain(&lines, field_domain).await?.len() < lines.len()
            {
                return Err(inaccessible());
            }
            lines.write(self, state.link_vals(record)).await?;
        }
        Ok(())
    }

    // ----- many2many ---------------------------------------------------------------------------

    async fn write_m2m(
        &mut self,
        model: ModelId,
        field: FieldId,
        groups: Vec<(Records, Vec<Command>)>,
    ) -> Result<()> {
        let env = groups[0].0.env().clone();
        let registry = Arc::clone(env.registry());
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let FieldType::Many2many {
            relation: Some(rel),
            ..
        } = &fs.ty
        else {
            return Ok(());
        };
        // links change on both sides of the relation (creates and inverse-side writes included)
        self.access_written(model, Some(&[field]));
        let co_env = self.command_env(&env, model, field, comodel);
        let co = Records::empty(co_env, comodel.id);
        let all: Vec<RecId> = {
            let mut seen = FxHashSet::default();
            groups
                .iter()
                .flat_map(|(r, _)| r.rec_ids().iter().copied())
                .filter(|i| seen.insert(*i))
                .collect()
        };
        let records = Records::from_parts(
            env.clone(),
            model,
            Arc::from(all.clone()),
            Arc::from(all.clone()),
        );
        self.ensure_x2many(&records, field).await?;

        // old relation (archived targets kept aside when the active test is on, REL §4.7.6);
        // visibility is decided once over the union of every record's targets
        let raws: Vec<(RecId, Vec<RecId>)> = all
            .iter()
            .map(|x| {
                let raw = match self.cache.get(model, field, *x) {
                    Some(Cell::Ids(ids)) => ids.to_vec(),
                    _ => Vec::new(),
                };
                (*x, raw)
            })
            .collect();
        let union: Vec<RecId> = {
            let mut seen = FxHashSet::default();
            raws.iter()
                .flat_map(|(_, raw)| raw.iter().copied())
                .filter(|y| seen.insert(*y))
                .collect()
        };
        let su_env = env.sudo(true);
        let visible_all = self.x2many_visible(&su_env, model, field, &union).await?;
        let mine: Option<FxHashSet<RecId>> = if !env.su() && !fs.bypass_search_access {
            Some(
                self.x2many_visible(&env, model, field, &visible_all)
                    .await?
                    .into_iter()
                    .collect(),
            )
        } else {
            None
        };
        let visible_all: FxHashSet<RecId> = visible_all.into_iter().collect();
        let active_test = env.context().active_test();
        let mut old: FxHashMap<RecId, OrderedIds> = FxHashMap::default();
        let mut old_inactive: FxHashMap<RecId, Vec<RecId>> = FxHashMap::default();
        let mut inaccessible: FxHashSet<RecId> = FxHashSet::default();
        for (x, raw) in raws {
            let visible: Vec<RecId> = raw
                .iter()
                .copied()
                .filter(|y| visible_all.contains(y))
                .collect();
            if active_test {
                old_inactive.insert(
                    x,
                    raw.iter()
                        .copied()
                        .filter(|y| !visible_all.contains(y))
                        .collect(),
                );
            }
            if let Some(mine) = &mine {
                inaccessible.extend(visible.iter().copied().filter(|y| !mine.contains(y)));
            }
            old.insert(x, OrderedIds(visible));
        }
        let mut new = old.clone();
        let mut added = OrderedIds::default();
        // creates, updates and deletes, batched across every record
        let mut to_create: Vec<(Vec<RecId>, RawVals)> = Vec::new();
        let mut to_update: Vec<(RawVals, Vec<RecId>)> = Vec::new();
        let mut to_delete: Vec<RecId> = Vec::new();
        for (recs, commands) in &groups {
            let xs: Vec<RecId> = recs.rec_ids().to_vec();
            let first_create = to_create.len();
            for command in commands {
                match command {
                    Command::Create(vals) => to_create.push((xs.clone(), vals.clone())),
                    Command::Update(id, vals) => match to_update.last_mut() {
                        Some((last, ids)) if last == vals => ids.push(*id),
                        _ => to_update.push((vals.clone(), vec![*id])),
                    },
                    Command::Delete(id) => to_delete.push(*id),
                    Command::Unlink(y) => {
                        if !inaccessible.contains(y) {
                            for x in &xs {
                                if let Some(set) = new.get_mut(x) {
                                    set.remove(*y);
                                }
                            }
                        }
                    }
                    Command::Link(y) => {
                        added.add(*y);
                        for x in &xs {
                            if let Some(set) = new.get_mut(x) {
                                set.add(*y);
                            }
                        }
                    }
                    Command::Clear | Command::Set(_) => {
                        // lines created earlier in this list are no longer linked to these records
                        for (targets, _) in &mut to_create[first_create..] {
                            targets.retain(|t| !xs.contains(t));
                        }
                        let ys: Vec<RecId> = match command {
                            Command::Set(ids) => ids.clone(),
                            _ => Vec::new(),
                        };
                        for y in &ys {
                            added.add(*y);
                        }
                        for x in &xs {
                            let mut set = OrderedIds::default();
                            for y in &ys {
                                set.add(*y);
                            }
                            if let Some(current) = new.get(x) {
                                for y in &current.0 {
                                    if inaccessible.contains(y) {
                                        set.add(*y);
                                    }
                                }
                            }
                            new.insert(*x, set);
                        }
                    }
                }
            }
        }
        for (vals, ids) in to_update {
            co.browse_ids(ids).write(self, vals).await?;
        }
        if !to_create.is_empty() {
            let (targets_of, vals): (Vec<Vec<RecId>>, Vec<RawVals>) = to_create.into_iter().unzip();
            let lines = co.create(self, vals).await?;
            for (line, targets) in lines.rec_ids().iter().zip(&targets_of) {
                added.add(*line);
                for x in targets {
                    if let Some(set) = new.get_mut(x) {
                        set.add(*line);
                    }
                }
            }
        }
        if !to_delete.is_empty() {
            let deleted: Vec<RecId> = to_delete
                .iter()
                .copied()
                .filter(|i| !inaccessible.contains(i))
                .collect();
            co.browse_ids(to_delete.iter().copied())
                .unlink(self)
                .await?;
            for set in old.values_mut().chain(new.values_mut()) {
                for y in &deleted {
                    set.remove(*y);
                }
            }
        }
        if !env.su() && !added.0.is_empty() {
            let co_env = co.env().clone();
            let lines =
                Records::empty(co_env.clone(), comodel.id).browse_ids(added.0.iter().copied());
            // a refused company context keeps its own error (review F1/S5)
            co_env.companies()?;
            let checked =
                match self.check_model(&co_env, comodel.id, crate::access::Operation::Read) {
                    Ok(()) => {
                        self.check_record_access(&lines, crate::access::Operation::Read)
                            .await
                    }
                    Err(error) => Err(error),
                };
            if let Err(error) = checked {
                return Err(if error.kind() == erp_fields::ErrorKind::Access {
                    Error::access(format!(
                        "Failed to write field {}.{}\n{}",
                        spec.name,
                        fs.name,
                        error.message()
                    ))
                    .with_source(error)
                } else {
                    error
                });
            }
        }
        // added targets must satisfy the field domain
        let field_domain = self.shared.relations.domain(model, field);
        if !field_domain.is_true() && !added.0.is_empty() {
            let lines = co.browse_ids(added.0.iter().copied());
            if self.filter_domain(&lines, &field_domain).await?.len() < lines.len() {
                return Err(Error::programming(format!(
                    "Cannot link inaccessible records in {}.{} ({field_domain})",
                    spec.name, fs.name
                )));
            }
        }
        if new == old {
            return Ok(());
        }
        // tracked many2many (D3): the id sets this write saw, no statement
        if spec.tracked.contains(&field) {
            for x in &all {
                let (o, n) = tracked_sets(spec.name, fs.name, *x, old.get(x), new.get(x))?;
                self.track_relation(spec, field, *x, &o.0, &n.0);
            }
        }
        // the relation table first: the cache only changes once the rows are written
        let mut add_pairs: Vec<(RecId, RecId)> = Vec::new();
        let mut remove_pairs: Vec<(RecId, RecId)> = Vec::new();
        for x in &all {
            let (o, n) = (
                old.get(x).cloned().unwrap_or_default(),
                new.get(x).cloned().unwrap_or_default(),
            );
            add_pairs.extend(n.0.iter().filter(|y| !o.contains(**y)).map(|y| (*x, *y)));
            remove_pairs.extend(o.0.iter().filter(|y| !n.contains(**y)).map(|y| (*x, *y)));
        }
        let relation = erp_sql::relational::Relation {
            table: &rel.table,
            column1: &rel.column1,
            column2: &rel.column2,
        };
        for (pairs, sql) in [
            (&add_pairs, erp_sql::relational::insert_pairs(relation)),
            (&remove_pairs, erp_sql::relational::delete_pairs(relation)),
        ] {
            // sorted, so that concurrent writers lock pairs in the same order; bounded chunks
            let mut sorted: Vec<(i64, i64)> =
                pairs.iter().map(|(x, y)| (x.get(), y.get())).collect();
            sorted.sort_unstable();
            for chunk in sorted.chunks(erp_sql::relational::PAIR_CHUNK) {
                let xs: Vec<i64> = chunk.iter().map(|(x, _)| *x).collect();
                let ys: Vec<i64> = chunk.iter().map(|(_, y)| *y).collect();
                self.execute_orm(&sql, &[&xs, &ys]).await?;
            }
        }
        // what the follow-delete triggers of the relation table did (a bumped cache): forgotten
        // here too, for this transaction (the statements above skip the SQL scan, review C3-R4)
        if !remove_pairs.is_empty() {
            self.forget_followed_table(&rel.table);
        }
        // cache: the new value, in command order (the comodel order is restored on the next
        // read), archived targets kept at the end
        let storage = fs.storage();
        for x in &all {
            let mut ids = new.get(x).map(|s| s.0.clone()).unwrap_or_default();
            if let Some(inactive) = old_inactive.get(x) {
                let extra: Vec<RecId> = inactive
                    .iter()
                    .copied()
                    .filter(|i| !ids.contains(i))
                    .collect();
                ids.extend(extra);
            }
            let set =
                self.cache
                    .model_mut(model)
                    .set(field, storage, *x, Cell::Ids(ids.into()), false);
            debug_assert!(set.is_ok(), "x2many cache column of another storage");
            if set.is_err() {
                self.cache
                    .model_mut(model)
                    .invalidate(Some(&[field][..]), Some(&[*x][..]));
            }
        }
        // reads made while linking may have memoized decisions from before the link changed
        self.access_written(model, Some(&[field]));
        self.patch_relation_caches(model, field, &records, &add_pairs, &remove_pairs)
            .await
    }

    /// Patch the cached values of sibling and inverse many2many fields after pairs changed, and
    /// notify their dependents (REL §4.7.6 step 8). Pairs are grouped by record first; domains are
    /// evaluated once per field for all the records concerned.
    async fn patch_relation_caches(
        &mut self,
        model: ModelId,
        field: FieldId,
        records: &Records,
        add_pairs: &[(RecId, RecId)],
        remove_pairs: &[(RecId, RecId)],
    ) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        let comodel = crate::relational::comodel_of(&registry, fs)?;
        let mut adds_by_x: FxHashMap<RecId, Vec<RecId>> = FxHashMap::default();
        let mut removes_by_x: FxHashMap<RecId, FxHashSet<RecId>> = FxHashMap::default();
        let mut adds_by_y: FxHashMap<RecId, Vec<RecId>> = FxHashMap::default();
        let mut removes_by_y: FxHashMap<RecId, FxHashSet<RecId>> = FxHashMap::default();
        for (x, y) in add_pairs {
            adds_by_x.entry(*x).or_default().push(*y);
            adds_by_y.entry(*y).or_default().push(*x);
        }
        for (x, y) in remove_pairs {
            removes_by_x.entry(*x).or_default().insert(*y);
            removes_by_y.entry(*y).or_default().insert(*x);
        }
        let mut touched: Vec<RecId> = adds_by_x
            .keys()
            .chain(removes_by_x.keys())
            .copied()
            .collect();
        touched.sort_unstable();
        touched.dedup();
        self.modified(model, vec![(field, touched.clone())]).await?;
        let no_ids: Vec<RecId> = Vec::new();
        let no_set: FxHashSet<RecId> = FxHashSet::default();
        // siblings: same model, same relation
        let siblings: Vec<FieldId> = self.shared.relations.siblings(model, field).to_vec();
        for sibling in siblings {
            let storage = spec.field_by_id(sibling).storage();
            let domain = self.shared.relations.domain(model, sibling);
            let cached: Vec<(RecId, Vec<RecId>)> = touched
                .iter()
                .filter_map(|x| match self.cache.get(model, sibling, *x) {
                    Some(Cell::Ids(current)) => Some((*x, current.to_vec())),
                    _ => None,
                })
                .collect();
            if cached.is_empty() {
                continue;
            }
            // the added targets satisfying the sibling's domain, decided once
            let mut seen = FxHashSet::default();
            let candidates: Vec<RecId> = cached
                .iter()
                .flat_map(|(x, _)| adds_by_x.get(x).unwrap_or(&no_ids).iter().copied())
                .filter(|y| seen.insert(*y))
                .collect();
            let valid: FxHashSet<RecId> = if candidates.is_empty() {
                FxHashSet::default()
            } else {
                let co = Records::from_parts(
                    records.env().sudo(true),
                    comodel.id,
                    Arc::from(candidates.clone()),
                    Arc::from(candidates),
                );
                self.filter_domain(&co, &domain)
                    .await?
                    .into_iter()
                    .collect()
            };
            for (x, mut value) in cached {
                for y in adds_by_x.get(&x).unwrap_or(&no_ids) {
                    if valid.contains(y) && !value.contains(y) {
                        value.push(*y);
                    }
                }
                let less = removes_by_x.get(&x).unwrap_or(&no_set);
                value.retain(|y| !less.contains(y));
                self.set_or_forget(model, sibling, storage, x, value);
            }
            self.modified(model, vec![(sibling, touched.clone())])
                .await?;
        }
        // inverses: the mirror field on the comodel
        let inverses: Vec<(ModelId, FieldId)> =
            self.shared.relations.inverses(model, field).to_vec();
        let mut coids: Vec<RecId> = adds_by_y
            .keys()
            .chain(removes_by_y.keys())
            .copied()
            .collect();
        coids.sort_unstable();
        coids.dedup();
        for (co_model, inverse) in inverses {
            let co_spec = registry.model_by_id(co_model);
            let storage = co_spec.field_by_id(inverse).storage();
            let domain = self.shared.relations.domain(co_model, inverse);
            let valid: FxHashSet<RecId> = self
                .filter_domain(&records.sudo(true), &domain)
                .await?
                .into_iter()
                .collect();
            for y in &coids {
                let Some(Cell::Ids(current)) = self.cache.get(co_model, inverse, *y) else {
                    continue;
                };
                let mut value: Vec<RecId> = current.to_vec();
                for x in adds_by_y.get(y).unwrap_or(&no_ids) {
                    if valid.contains(x) && !value.contains(x) {
                        value.push(*x);
                    }
                }
                let less = removes_by_y.get(y).unwrap_or(&no_set);
                value.retain(|x| !less.contains(x));
                self.set_or_forget(co_model, inverse, storage, *y, value);
            }
            self.modified(co_model, vec![(inverse, coids.clone())])
                .await?;
        }
        Ok(())
    }

    /// Cache an x2many value; a cache column of another storage (a bug) forgets it instead.
    fn set_or_forget(
        &mut self,
        model: ModelId,
        field: FieldId,
        storage: erp_fields::Storage,
        id: RecId,
        value: Vec<RecId>,
    ) {
        let mc = self.cache.model_mut(model);
        let set = mc.set(field, storage, id, Cell::Ids(value.into()), false);
        debug_assert!(set.is_ok(), "x2many cache column of another storage");
        if set.is_err() {
            mc.invalidate(Some(&[field][..]), Some(&[id][..]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: i64) -> RecId {
        RecId::from_real(i).unwrap_or_else(|| unreachable!())
    }

    /// r1 M1: a record without an old or new id set is an error naming it,
    /// never a silent skip.
    #[test]
    fn tracked_sets_refuses_a_missing_set_loudly() {
        let set = OrderedIds(vec![id(2)]);
        let ok = tracked_sets("m.t", "tag_ids", id(1), Some(&set), Some(&set));
        assert!(ok.is_ok());
        for (o, n, which) in [(None, Some(&set), "old"), (Some(&set), None, "new")] {
            let Err(e) = tracked_sets("m.t", "tag_ids", id(1), o, n) else {
                unreachable!("a missing set must be an error");
            };
            assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
            assert!(e.message().contains("m.t.tag_ids"), "{}", e.message());
            assert!(
                e.message().contains(&format!("no {which} id set")),
                "{}",
                e.message()
            );
            assert!(e.message().contains("record"), "{}", e.message());
        }
    }

    #[test]
    fn spec_rel_043_command_triples() {
        let v = RawVals::new().set_cell("name", Cell::Str("x".into()));
        let mut map = erp_fields::ValueMap::new();
        map.insert("name".to_owned(), Value::str("x"));
        let vals = Value::Map(Arc::new(map));
        assert_eq!(
            Command::create(v.clone()).to_value(),
            Value::list([Value::Int(0), Value::Int(0), vals.clone()])
        );
        assert_eq!(
            Command::update(id(5), v).to_value(),
            Value::list([Value::Int(1), Value::Int(5), vals])
        );
        for (cmd, code) in [
            (Command::delete(id(5)), 2),
            (Command::unlink(id(5)), 3),
            (Command::link(id(5)), 4),
        ] {
            assert_eq!(
                cmd.to_value(),
                Value::list([Value::Int(code), Value::Int(5), Value::Int(0)])
            );
        }
        assert_eq!(
            Command::clear().to_value(),
            Value::list([Value::Int(5), Value::Int(0), Value::Int(0)])
        );
        assert_eq!(
            Command::set([id(1), id(2)]).to_value(),
            Value::list([
                Value::Int(6),
                Value::Int(0),
                Value::list([Value::Int(1), Value::Int(2)])
            ])
        );
    }
}
