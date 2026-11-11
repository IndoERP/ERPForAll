//! The closed event namespace (A2-D5): relation rules of a model whose table lives in the
//! event schema. Selecting the namespace, extension and inheritance are refused by the composer.

use crate::drop::{DropReason, Sink};
use crate::spec::{ModelSpec, TableNamespace};
use erp_fields::{Error, FieldType, Result};
use rustc_hash::FxHashSet;

/// An event model is a closed leaf: no many2one or many2many of another model may point to it (an
/// `_inherits` delegate is one), and it takes no relational field of its own (a foreign key or a
/// relation table would tie its table to the `public` schema). Soft references (`res_model` /
/// `res_id`, `Reference` fields) carry the model in data and are allowed. Runs once the relational
/// setup is done.
///
/// A custom (`x_`) field that references an event model is refused like any broken custom field.
///
/// # Errors
///
/// `Programming` naming the event model and the first offending field, in registry and field order.
pub(crate) fn check_references(models: &[ModelSpec], sink: &mut Sink) -> Result<()> {
    let events: FxHashSet<&str> = models
        .iter()
        .filter(|m| m.namespace == TableNamespace::Event)
        .map(|m| m.name)
        .collect();
    if events.is_empty() {
        return Ok(());
    }
    for model in models {
        let own = events.contains(model.name);
        for field in &model.fields {
            // an `_inherits` copy is reported through the parent's own field
            if field.inherited.is_some() {
                continue;
            }
            let target = match &field.ty {
                FieldType::Many2one { comodel, .. }
                | FieldType::Many2many { comodel, .. }
                | FieldType::One2many { comodel, .. } => comodel.as_str(),
                _ => continue,
            };
            let error = if own {
                Error::programming(format!(
                    "model '{}' is in the closed event namespace and takes no relational field; \
                     '{}.{}' is one",
                    model.name, model.name, field.name
                ))
            } else if events.contains(target) && !matches!(field.ty, FieldType::One2many { .. }) {
                Error::programming(format!(
                    "model '{target}' is in the closed event namespace, so no field may reference \
                     it; '{}.{}' does",
                    model.name, field.name
                ))
            } else {
                continue;
            };
            sink.field(&model.mro, field, DropReason::Refused, error)?;
        }
    }
    Ok(())
}
