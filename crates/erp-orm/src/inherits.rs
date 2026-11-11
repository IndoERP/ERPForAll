//! `_inherits` delegation at run time (MODEL §4.4, CRUD §4.2 step 5, §4.4; REL §4.4.7).
//!
//! An inherited field has no storage of its own: reading it reads the parent's field through the
//! delegate many2one (several levels deep when the parent's field is itself inherited,
//! values, one batched `create` per parent model; creating with the delegate writes the given
//! inherited values on that parent. Writing inherited values writes the parents, after the
//! child's own values (so a new delegate in the same write is honoured,/025).

use std::sync::Arc;

use erp_fields::{Cell, Result};
use erp_registry::{FieldId, ModelSpec};
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::records::Records;
use crate::vals::RawVals;

impl Transaction {
    /// The cells of an inherited field for `records`, read on their parents.
    pub(crate) fn inherited_cells<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
    ) -> crate::dispatch::BoxFuture<'a, Result<Vec<Cell>>> {
        // boxed: reading the parent's field may recurse (several levels of `_inherits`)
        Box::pin(self.inherited_cells_inner(records, field))
    }

    async fn inherited_cells_inner(
        &mut self,
        records: &Records,
        field: FieldId,
    ) -> Result<Vec<Cell>> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let Some(inherited) = spec.field_by_id(field).inherited else {
            return Ok(vec![Cell::Null; records.len()]);
        };
        let delegate = spec.field_by_id(inherited.delegate).name;
        let parents = records.related(self, delegate).await?;
        let parent_spec = registry.model_by_id(inherited.parent);
        let parent_field = parent_spec.field_by_id(inherited.parent_field).name;
        let cells = parents.cells(self, parent_field).await?;
        let by_parent: FxHashMap<RecId, Cell> =
            parents.rec_ids().iter().copied().zip(cells).collect();
        let links = records.cells(self, delegate).await?;
        Ok(links
            .iter()
            .map(|link| {
                crate::relational::id_of(link)
                    .and_then(|p| by_parent.get(&p).cloned())
                    .unwrap_or(Cell::Null)
            })
            .collect())
    }
}

use erp_fields::RecId;

/// Values of inherited fields split out of `vals`, per delegate field (cells and commands).
pub(crate) fn split_inherited(spec: &ModelSpec, vals: &mut RawVals) -> Vec<(FieldId, RawVals)> {
    let mut out: Vec<(FieldId, RawVals)> = Vec::new();
    let names: Vec<String> = vals.iter().map(|(n, _)| n.to_owned()).collect();
    for name in names {
        let Some(inherited) = spec.field(&name).and_then(|f| f.inherited) else {
            continue;
        };
        let Some(cell) = vals.remove(&name) else {
            continue;
        };
        let slot = match out.iter().position(|(d, _)| *d == inherited.delegate) {
            Some(i) => i,
            None => {
                out.push((inherited.delegate, RawVals::new()));
                out.len() - 1
            }
        };
        out[slot].1.insert(name.into(), cell);
    }
    let names: Vec<String> = vals.iter_commands().map(|(n, _)| n.to_owned()).collect();
    for name in names {
        let Some(inherited) = spec.field(&name).and_then(|f| f.inherited) else {
            continue;
        };
        let Some(commands) = vals.remove_commands(&name) else {
            continue;
        };
        let slot = match out.iter().position(|(d, _)| *d == inherited.delegate) {
            Some(i) => i,
            None => {
                out.push((inherited.delegate, RawVals::new()));
                out.len() - 1
            }
        };
        out[slot].1.insert_commands(name.into(), commands);
    }
    out
}
