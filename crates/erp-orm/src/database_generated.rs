//! Database-owned output boundaries (/D5).
use erp_fields::{Error, RecId, Result};
use erp_registry::{FieldSpec, ModelSpec};

pub(crate) fn check_assignment(model: &ModelSpec, field: &FieldSpec) -> Result<()> {
    if field.is_generated() {
        return Err(Error::validation(format!(
            "Generated field '{}.{}' is read-only",
            model.name, field.name
        )));
    }
    Ok(())
}

pub(crate) fn check_persisted(model: &ModelSpec, field: &FieldSpec, ids: &[RecId]) -> Result<()> {
    if field.is_generated() && ids.iter().any(|id| id.is_new()) {
        return Err(Error::programming(format!(
            "Generated field '{}.{}' requires a persisted record",
            model.name, field.name
        )));
    }
    Ok(())
}

/// Editable related fields forward writes. Refuse a generated terminal before any override,
/// inverse, inherited-parent creation or cache mutation can receive the assignment.
pub(crate) fn check_write_names<'a>(
    tx: &crate::Transaction,
    model: &ModelSpec,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    for name in names {
        let Some(field) = model.field(name) else {
            // The operation owns its existing unknown-field error; this guard only adds generation.
            continue;
        };
        check_assignment(model, field)?;
        if !field.readonly
            && let Some(read) = tx.shared.graph.generated_read((model.id, field.id))
        {
            let (owner, target) = read.path.last().ok_or_else(|| {
                Error::programming("Generated write-through alias has no owning target")
            })?;
            let owner = tx.registry().model_by_id(*owner);
            let target = owner.field_by_id(*target);
            if !target.is_generated() {
                return Err(Error::programming(
                    "Generated write-through alias has a non-generated owning target",
                ));
            }
            check_assignment(owner, target)?;
        }
    }
    Ok(())
}
