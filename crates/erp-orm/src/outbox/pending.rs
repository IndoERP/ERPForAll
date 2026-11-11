//! Bounded attempt-local provenance. Cells hold immutable writer context, never field values.

use super::CaptureContext;
use erp_fields::{Error, RecId, Result};
use erp_registry::{FieldId, ModelId};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Conservative retained-memory ceiling, separate from the encoded-event buffer ceiling.
const MAX_PENDING_BYTES: usize = super::MAX_TRANSACTION_BYTES;
// Conservatively retain 128 bytes for the cell tree and 64 for its admission-set slot.
// The temporary set and retained tree can coexist while the admitted keys are inserted.
const ENTRY_OVERHEAD: usize = 192;
type Key = (ModelId, FieldId, RecId);

#[derive(Debug, Clone, Default)]
pub(crate) struct Pending {
    cells: BTreeMap<Key, Arc<CaptureContext>>,
    bytes: usize,
    reserved_bytes: usize,
    /// An execution scope only; deferred work always consults `cells` first.
    pub(crate) active: Option<Arc<CaptureContext>>,
}

impl Pending {
    pub(crate) fn has_cells(&self) -> bool {
        !self.cells.is_empty()
    }

    pub(crate) fn bind(&mut self, model: ModelId, field: FieldId, ids: &[RecId]) -> Result<()> {
        if ids.iter().all(|id| id.is_new()) {
            return Ok(());
        }
        let origin = self.active.as_ref().ok_or_else(|| {
            Error::programming("outbox: event-relevant pending work is missing writer provenance")
        })?;
        let charge = origin
            .pending_charge()?
            .checked_add(ENTRY_OVERHEAD)
            .ok_or_else(|| super::limit("pending provenance accounting overflow"))?;
        let mut bytes = self.bytes;
        let mut fresh = BTreeSet::new();
        // No copy of the caller's ID array. Admit each distinct new key before retaining it;
        // existing keys and duplicate input IDs cost no new slot. No cells change on failure.
        for id in ids.iter().filter(|id| !id.is_new()) {
            if let Some(existing) = self.cells.get(&(model, field, *id)) {
                if !Arc::ptr_eq(existing, origin) {
                    return Err(Error::programming(
                        "outbox: pending provenance would overwrite an unsettled operation",
                    ));
                }
            } else if !fresh.contains(id) {
                bytes = bytes
                    .checked_add(charge)
                    .ok_or_else(|| super::limit("pending provenance accounting overflow"))?;
                if bytes
                    .checked_add(self.reserved_bytes)
                    .is_none_or(|total| total > MAX_PENDING_BYTES)
                {
                    return Err(super::limit("pending provenance exceeds 8388608 bytes"));
                }
                fresh.insert(*id);
            }
        }
        for id in fresh {
            self.cells.insert((model, field, id), Arc::clone(origin));
        }
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn origin(
        &self,
        model: ModelId,
        field: FieldId,
        id: RecId,
    ) -> Option<Arc<CaptureContext>> {
        self.cells.get(&(model, field, id)).cloned()
    }

    /// Charge the copy retained by an isolated scope before it can be allocated.
    pub(crate) fn snapshot(&mut self) -> Result<Self> {
        let total = self
            .reserved_bytes
            .checked_add(self.bytes)
            .ok_or_else(|| super::limit("pending provenance accounting overflow"))?;
        if total
            .checked_add(self.bytes)
            .is_none_or(|sum| sum > MAX_PENDING_BYTES)
        {
            return Err(super::limit(
                "isolated pending provenance exceeds 8388608 bytes",
            ));
        }
        let snapshot = self.clone();
        self.reserved_bytes = total;
        Ok(snapshot)
    }

    /// Drop provenance only when its dirty/pending cached business cell no longer exists.
    /// Snapshot reservations remain charged until the owning isolated scope is restored.
    pub(crate) fn prune(&mut self, cache: &crate::cache::Cache) -> Result<()> {
        let live = |key: &Key| {
            cache
                .model(key.0)
                .is_some_and(|model| model.is_dirty(key.1, key.2) || model.is_pending(key.1, key.2))
        };
        let mut bytes = 0usize;
        for (key, origin) in &self.cells {
            if live(key) {
                bytes = bytes
                    .checked_add(origin.pending_charge()?)
                    .and_then(|n| n.checked_add(ENTRY_OVERHEAD))
                    .ok_or_else(|| super::limit("pending provenance accounting overflow"))?;
            }
        }
        self.cells.retain(|key, _| live(key));
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn clear_cells(&mut self) {
        self.cells.clear();
        self.bytes = 0;
    }
    pub(crate) fn clear(&mut self) {
        self.clear_cells();
        self.active = None;
        self.reserved_bytes = 0;
    }
}

impl crate::Transaction {
    /// Reachability is structural, so identity admission happens before any input is overwritten.
    pub(crate) fn capture_relevant(&self, model: ModelId, field: FieldId) -> bool {
        let mut todo = vec![(model, field)];
        let mut seen = rustc_hash::FxHashSet::default();
        while let Some(key) = todo.pop() {
            if !seen.insert(key) {
                continue;
            }
            let spec = self.registry().model_by_id(key.0);
            let fs = spec.field_by_id(key.1);
            // Every physical flush updates write_date. Its dependents must retain this
            // source's origin even when the explicitly assigned field has no ordinary edge.
            if spec.log_access
                && fs.has_column()
                && fs.inherited.is_none()
                && let Some(audit) = spec.field("write_date")
            {
                todo.push((key.0, audit.id));
            }
            if let Some(inherited) = fs.inherited {
                todo.push((inherited.parent, inherited.parent_field));
            }
            if spec.eventful && fs.has_column() && fs.inherited.is_none() {
                return true;
            }
            todo.extend(
                self.shared
                    .graph
                    .model(key.0)
                    .dependents(key.1)
                    .iter()
                    .map(|f| (key.0, *f)),
            );
            if let Some(tree) = self.shared.graph.triggers(key) {
                todo.extend(tree.all_targets.iter().copied());
            }
            for output in &spec.fields {
                if output
                    .generated()
                    .is_some_and(|generated| generated.dependencies().contains(&key.1))
                {
                    todo.push((key.0, output.id));
                }
            }
        }
        false
    }

    pub(crate) fn capture_model_relevant(&self, model: ModelId) -> bool {
        self.registry()
            .model_by_id(model)
            .fields
            .iter()
            .any(|f| self.capture_relevant(model, f.id))
    }

    pub(crate) fn check_capture_create(
        &mut self,
        records: &crate::Records,
        rows: &[crate::RawVals],
    ) -> Result<()> {
        self.check_records(records)?;
        if rows.is_empty() || !self.capture_model_relevant(records.model_id()) {
            return Ok(());
        }
        if self.pending_capture.active.is_some() {
            let error = Error::programming(
                "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented",
            );
            // An addon may catch this refusal inside its inverse/compute. The outer business
            // work is still rollback-only; returning an error alone cannot enforce that.
            self.poison_with(&error);
            return Err(error);
        }
        if records.env().operation_identity().is_none() {
            return Err(Error::programming(
                "outbox: eventful mutation requires an OperationIdentity",
            ));
        }
        if rows.iter().any(|row| {
            row.iter_commands().next().is_some()
                || row
                    .iter()
                    .any(|(name, _)| records.spec().field(name).is_some_and(|f| f.ty.is_x2many()))
        }) {
            return Err(Error::programming(format!(
                "outbox: eventful relation capture for model '{}' is not implemented until S7-S9",
                records.spec().name
            )));
        }
        Ok(())
    }

    pub(crate) fn capture_write_relevant(
        &self,
        records: &crate::Records,
        vals: &crate::RawVals,
    ) -> bool {
        records.spec().eventful
            || vals
                .names()
                .chain(
                    ["write_uid", "write_date"]
                        .into_iter()
                        .filter(|_| records.spec().log_access),
                )
                .filter_map(|name| records.spec().field_id(name))
                .any(|field| self.capture_relevant(records.model_id(), field))
    }

    pub(crate) fn check_capture_write(
        &mut self,
        records: &crate::Records,
        vals: &crate::RawVals,
    ) -> Result<()> {
        self.check_records(records)?;
        if records.is_empty() {
            return Ok(());
        }
        let relevant = self.capture_write_relevant(records, vals);
        if !relevant {
            return Ok(());
        }
        if self.pending_capture.active.is_some() {
            let error = Error::programming(
                "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented",
            );
            // An addon may catch this refusal inside its inverse/compute. The outer business
            // work is still rollback-only; returning an error alone cannot enforce that.
            self.poison_with(&error);
            return Err(error);
        }
        if records.env().operation_identity().is_none() {
            return Err(Error::programming(
                "outbox: eventful mutation requires an OperationIdentity",
            ));
        }
        if vals.iter_commands().next().is_some()
            || vals
                .iter()
                .any(|(n, _)| records.spec().field(n).is_some_and(|f| f.ty.is_x2many()))
        {
            return Err(Error::programming(format!(
                "outbox: eventful relation capture for model '{}' is not implemented until S7-S9",
                records.spec().name
            )));
        }
        Ok(())
    }

    pub(crate) async fn begin_capture_operation(
        &mut self,
        records: &crate::Records,
        relevant: bool,
    ) -> Result<()> {
        self.check_records(records)?;
        if !relevant {
            return Ok(());
        }
        if self.pending_capture.active.is_some() {
            let error = Error::programming(
                "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented",
            );
            // An addon may catch this refusal inside its inverse/compute. The outer business
            // work is still rollback-only; returning an error alone cannot enforce that.
            self.poison_with(&error);
            return Err(error);
        }
        if records.env().operation_identity().is_none() {
            return Err(Error::programming(
                "outbox: eventful mutation requires an OperationIdentity",
            ));
        }
        if self.pending_capture.has_cells() {
            if let Err(error) = Box::pin(self.flush()).await {
                self.poison_with(&error);
                return Err(error);
            }
            self.pending_capture.clear_cells();
        }
        let companies = self
            .companies(records.env())
            .await?
            .into_iter()
            .map(|id| id.get())
            .collect();
        self.pending_capture.active = Some(Arc::new(CaptureContext::from_env(
            records.env(),
            companies,
        )?));
        Ok(())
    }

    pub(crate) fn bind_capture(
        &mut self,
        model: ModelId,
        field: FieldId,
        ids: &[RecId],
    ) -> Result<()> {
        if self.capture_relevant(model, field) {
            self.pending_capture.bind(model, field, ids)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    fn fixture() -> (ModelId, FieldId, Arc<CaptureContext>) {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "pending",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl =
            ModelDecl::new("pending", "pending.item").fields(&[FieldDecl::char("name")]);
        let registry = Arc::new(
            RegistryBuilder::new([&ADDON])
                .with_decls([&MODEL])
                .build()
                .unwrap(),
        );
        let spec = registry.model("pending.item").unwrap();
        let key = (spec.id, spec.field_id("name").unwrap());
        let env = crate::Env::new(
            registry,
            crate::Principal::for_uid(crate::UserId::new(2).unwrap()),
            crate::Context::new(),
        )
        .with_operation_identity(Some(Arc::new(
            crate::OperationIdentity::generate().unwrap(),
        )));
        (
            key.0,
            key.1,
            Arc::new(CaptureContext::from_env(&env, vec![]).unwrap()),
        )
    }

    #[test]
    fn pruning_retains_surviving_dirty_and_pending_origins_and_snapshot_reservations() {
        let (model, field, origin) = fixture();
        let ids: Vec<_> = (1..=3).map(|id| RecId::from_real(id).unwrap()).collect();
        let mut pending = Pending {
            active: Some(origin),
            ..Pending::default()
        };
        pending.bind(model, field, &ids).unwrap();
        let snapshot = pending.snapshot().unwrap();
        let reservation = pending.reserved_bytes;
        let mut cache = crate::cache::Cache::default();
        cache
            .model_mut(model)
            .set(
                field,
                erp_fields::Storage::Str,
                ids[0],
                erp_fields::Cell::Str("dirty".into()),
                true,
            )
            .unwrap();
        cache
            .model_mut(model)
            .mark_pending(field, erp_fields::Storage::Str, &ids[1..2]);
        pending.prune(&cache).unwrap();
        assert!(pending.origin(model, field, ids[0]).is_some());
        assert!(pending.origin(model, field, ids[1]).is_some());
        assert!(pending.origin(model, field, ids[2]).is_none());
        assert_eq!(pending.reserved_bytes, reservation);
        assert_eq!(pending.bytes * 3, snapshot.bytes * 2);
        assert_eq!(snapshot.cells.len(), 3);
    }

    #[test]
    fn missing_or_overwritten_origin_fails_loudly() {
        let (m, f, origin) = fixture();
        let id = RecId::from_real(1).unwrap();
        let mut pending = Pending::default();
        let error = pending.bind(m, f, &[id]).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: event-relevant pending work is missing writer provenance"
        );
        pending.active = Some(Arc::clone(&origin));
        pending.bind(m, f, &[id]).unwrap();
        let bytes = pending.bytes;
        pending.bind(m, f, &[id]).unwrap();
        assert_eq!(pending.bytes, bytes);
        pending.active = Some(fixture().2);
        let error = pending.bind(m, f, &[id]).unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: pending provenance would overwrite an unsettled operation"
        );
        assert!(Arc::ptr_eq(&pending.origin(m, f, id).unwrap(), &origin));
    }

    #[test]
    fn pending_limit_is_atomic_and_snapshot_charge_is_restored() {
        let (m, f, origin) = fixture();
        let mut pending = Pending {
            active: Some(origin),
            ..Pending::default()
        };
        pending.bind(m, f, &[RecId::from_real(1).unwrap()]).unwrap();
        let snapshot = pending.snapshot().unwrap();
        assert_eq!(pending.reserved_bytes, snapshot.bytes);
        let reserved = pending.reserved_bytes;
        pending.clear_cells();
        assert_eq!(pending.reserved_bytes, reserved);
        pending = snapshot;
        assert_eq!(pending.reserved_bytes, 0);
        pending.reserved_bytes = MAX_PENDING_BYTES - pending.bytes;
        let before = pending.bytes;
        let error = pending
            .bind(m, f, &[RecId::from_real(2).unwrap()])
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "outbox CaptureLimit: pending provenance exceeds 8388608 bytes"
        );
        assert_eq!(pending.bytes, before);
        assert!(pending.origin(m, f, RecId::from_real(2).unwrap()).is_none());
        let error = pending.snapshot().unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "outbox CaptureLimit: isolated pending provenance exceeds 8388608 bytes"
        );
        pending.clear();
        assert!(!pending.has_cells());
        assert_eq!(pending.bytes, 0);
        assert_eq!(pending.reserved_bytes, 0);
    }
    #[test]
    fn duplicate_ids_admit_once_at_pending_limit() {
        let (m, f, origin) = fixture();
        let charge = origin.pending_charge().unwrap() + ENTRY_OVERHEAD;
        let mut pending = Pending {
            active: Some(origin),
            reserved_bytes: MAX_PENDING_BYTES - charge,
            ..Pending::default()
        };
        let id = RecId::from_real(1).unwrap();
        pending.bind(m, f, &[id, id, id]).unwrap();
        assert_eq!(pending.bytes, charge);
        assert_eq!(pending.cells.len(), 1);
        let error = pending
            .bind(m, f, &[RecId::from_real(2).unwrap()])
            .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            error.message(),
            "outbox CaptureLimit: pending provenance exceeds 8388608 bytes"
        );
        assert_eq!(pending.cells.len(), 1);
    }
}
