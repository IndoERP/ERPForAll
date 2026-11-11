//! Module-data view-type admission; the transaction supplies lifecycle authority.

use erp_sdk::views::PublishedViewTypes;
use erp_sdk::{Context, Env, Error, Result, Transaction, Value};
use std::sync::Arc;

pub(super) fn loading_module(managed: bool, context: &Context) -> Result<Option<Arc<str>>> {
    if !managed {
        return Ok(None);
    }
    match context.get(erp_sdk::load_state::DECLARING_MODULE) {
        None => Ok(None),
        Some(Value::Str(module)) => Ok(Some(module)),
        Some(_) => Err(Error::programming(
            "H18 declaring module context __erp_data_module must be text",
        )),
    }
}

pub(crate) fn require_loaded_view_type(tx: &Transaction, env: &Env, key: &str) -> Result<()> {
    let Some(module) = loading_module(tx.is_operation_managed(), env.context())? else {
        return Ok(());
    };
    tx.require_system_full("H18 module view admission")?;
    // These are framework schemas (plus the separate template kind), not addon-owned data views.
    if matches!(key, "list" | "form" | "search" | "qweb") {
        return Ok(());
    }
    let table = env.generation_attachment::<PublishedViewTypes>()?;
    table
        .require_registry(env.registry())
        .map_err(|error| Error::programming(error.to_string()))?;
    table
        .require_key_for(&module, key)
        .map_err(|error| Error::validation(error.to_string()))
}
