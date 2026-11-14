// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native notification after SQL/trigger changes. No writer or guest route.

use std::sync::Arc;

use erp_fields::{Error, FieldType, Result};
use rustc_hash::FxHashSet;

use crate::{Records, Transaction};

impl Records {
    /// Notify that external SQL changed these records' stored scalar columns in this transaction.
    ///
    /// Settle pending ORM work **before** executing the SQL, then notify the affected records and
    /// columns. This call drops their cached values and uses the ordinary write dependency walk:
    /// stored dependents become pending; non-stored dependents are invalidated in every context,
    /// including across unchanged relations. Later ordinary reads/flush/commit settle computes.
    ///
    /// This final native-only operation does not authorize the SQL or grant any read/write right.
    /// It performs no writes or flush, invokes no write overrides, runs no model constraints on
    /// the notified fields and creates no source audit/event record. The SQL writer owns those
    /// obligations, validation included. Dependency traversal may SELECT relation
    /// edges. No RPC, scripting or WASM entry is registered.
    ///
    /// All names are checked even on an empty recordset. Empty admitted input is a no-op;
    /// duplicate names and ids are coalesced. Resolve pending principal switches before the SQL:
    /// notification validates provenance synchronously, without a flushing resolution path.
    ///
    /// # Errors
    ///
    /// `Programming` for unknown/non-column/delegated fields, relational sources (awaiting
    /// ), draft ids or dirty/pending source cells, naming
    /// the invalid field/record. Record binding, scope and provenance errors retain their kinds.
    /// Propagation errors are returned and poison the transaction; cancellation after invalidation
    /// also leaves it rollback-only. An admission refusal invalidates nothing. Read-only generated
    /// input callbacks refuse notification; a caught refusal stays fatal to that callback.
    pub async fn modified_external(&self, tx: &mut Transaction, fields: &[&str]) -> Result<()> {
        // Synchronous admission is deliberate: admit_records may resolve switches and flush.
        tx.check_records(self)?;
        // Notification mutates cache/compute state even though it sends no write.
        tx.refuse_adapter_effect()?;
        let spec = self.spec();
        let mut seen = FxHashSet::default();
        let mut field_ids = Vec::new();
        for name in fields {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!(
                    "External modification: unknown field '{}.{name}'",
                    spec.name
                ))
            })?;
            if field.column().is_none() || field.inherited.is_some() {
                return Err(Error::programming(format!(
                    "External modification: field '{}.{name}' is not a stored column owned by this model",
                    spec.name
                )));
            }
            if matches!(
                field.ty,
                FieldType::Many2one { .. }
                    | FieldType::Reference { .. }
                    | FieldType::Many2oneReference { .. }
            ) {
                return Err(Error::programming(format!(
                    "External modification: relational source '{}.{name}' requires a later release",
                    spec.name
                )));
            }
            if seen.insert(field.id) {
                field_ids.push(field.id);
            }
        }
        let mut seen = FxHashSet::default();
        let mut ids = Vec::new();
        for id in self.rec_ids() {
            if id.is_new() {
                return Err(Error::programming(format!(
                    "External modification: draft record {id} of '{}' has no database row",
                    spec.name
                )));
            }
            if seen.insert(*id) {
                ids.push(*id);
            }
        }
        if let Some(cache) = tx.cache.model(spec.id) {
            for field in &field_ids {
                if ids
                    .iter()
                    .any(|id| cache.is_dirty(*field, *id) || cache.is_pending(*field, *id))
                {
                    return Err(Error::programming(format!(
                        "External modification: field '{}.{}' has pending ORM work; settle it before external SQL",
                        spec.name,
                        spec.field_by_id(*field).name
                    )));
                }
            }
        }
        if field_ids.is_empty() || ids.is_empty() {
            return Ok(());
        }
        // From here an error or dropped future must not allow a partial notification to commit.
        let guard = tx.guard();
        tx.cache
            .model_mut(spec.id)
            .invalidate(Some(&field_ids), Some(&ids));
        tx.engine
            .context_mut()
            .invalidate(spec.id, Some(&field_ids), Some(&ids));
        tx.note_database_change(spec.id, &field_ids);
        tx.write_gens.model_written(spec.id);
        let changes = field_ids
            .into_iter()
            .map(|field| (field, ids.clone()))
            .collect();
        let result = tx
            .modified_prefetch(spec.id, changes, Some(Arc::from(self.prefetch_ids())))
            .await;
        if let Err(error) = &result {
            tx.poison_with(error);
        }
        guard.disarm();
        result
    }
}
