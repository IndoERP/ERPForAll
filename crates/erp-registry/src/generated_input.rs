// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pure declaration expectations for generated input adapters.
use crate::Registry;
use erp_fields::{Error, Result};

/// Registry-side declaration of an ordinary native `_adapt_generated_input` method.
/// Executable readiness is checked independently by OrmBuilder.
#[derive(Debug)]
#[non_exhaustive]
pub struct GeneratedInputAdapterDecl {
    addon: &'static str,
    model: &'static str,
}
impl GeneratedInputAdapterDecl {
    /// Declare the defining addon and model. Metadata grants no runtime capability.
    pub const fn new(addon: &'static str, model: &'static str) -> Self {
        Self { addon, model }
    }
}
/// Definitions emitted by the native methods macro; overrides use the ordinary method chain.
#[cfg(not(target_family = "wasm"))]
#[linkme::distributed_slice]
pub static GENERATED_INPUT_ADAPTER_DECLS: [GeneratedInputAdapterDecl];
/// Guest registries have no native executable adapter declarations.
#[cfg(target_family = "wasm")]
pub static GENERATED_INPUT_ADAPTER_DECLS: &[GeneratedInputAdapterDecl] = &[];

pub(crate) fn validate(
    registry: &Registry,
    extra: &[&GeneratedInputAdapterDecl],
    linked: bool,
) -> Result<()> {
    for model in registry.models() {
        let mut adapted = false;
        for field in &model.fields {
            if !field.input_adapted {
                continue;
            }
            if !field.is_generated() {
                return Err(Error::programming(format!(
                    "Invalid generated input field '{}.{}': input_adapted requires a generated field",
                    model.name, field.name
                )));
            }
            if field.inherited.is_some() {
                continue;
            }
            adapted = true;
        }
        if adapted
            && !extra
                .iter()
                .copied()
                .chain(GENERATED_INPUT_ADAPTER_DECLS.iter().filter(|_| linked))
                .any(|decl| {
                    model
                        .mro
                        .iter()
                        .any(|m| m.name == decl.model && m.addon == decl.addon)
                })
        {
            return Err(Error::programming(format!(
                "Model '{}' has input-adapted generated fields but no _adapt_generated_input declaration",
                model.name
            )));
        }
    }
    Ok(())
}
