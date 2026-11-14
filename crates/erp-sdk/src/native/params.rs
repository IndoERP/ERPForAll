// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared technical system parameters: registration and typed reads.
//!
//! An addon declares its bounded `ir.config_parameter` keys with [`crate::system_parameters!`];
//! base validates every write path against them, ships a default row per key, and the addon reads
//! them with [`get_int`], [`get_float`], [`get_bool`] and [`get_str`].,
//! "Technical parameters".
use crate::settings::{ParameterContributions, ParameterDecl, ParameterRegistry, SettingGroupDecl};
use crate::{AddonDescriptor, Env, Error, Registry, Result, Transaction, Value};
use std::collections::BTreeSet;

/// One addon's parameter and parameter-group factories, bound to its descriptor.
#[derive(Debug)]
#[non_exhaustive]
pub struct ParameterContribution {
    /// Native addon identity, including its dependency declarations.
    pub addon: &'static AddonDescriptor,
    /// Pure validated parameter declarations.
    pub declarations: fn() -> Result<Vec<ParameterDecl>>,
    /// Pure group validators over those parameters.
    pub groups: fn() -> Result<Vec<SettingGroupDecl>>,
}
impl ParameterContribution {
    /// Bind the two factories to a native addon descriptor.
    pub const fn new(
        addon: &'static AddonDescriptor,
        declarations: fn() -> Result<Vec<ParameterDecl>>,
        groups: fn() -> Result<Vec<SettingGroupDecl>>,
    ) -> Self {
        Self {
            addon,
            declarations,
            groups,
        }
    }
}

/// Parameter contributions linked into the binary; only installed addons' entries are published.
#[linkme::distributed_slice]
pub static PARAMETER_CONTRIBUTIONS: [ParameterContribution];

/// A group factory for [`crate::system_parameters!`] when an addon has no parameter groups.
/// # Errors
/// Never.
pub fn no_groups() -> Result<Vec<SettingGroupDecl>> {
    Ok(Vec::new())
}

/// Register this addon's technical parameters (and optionally their group validators) using its
/// generated `ADDON` identity.
///
/// ```rust,ignore
/// use erp_sdk::settings::{ParameterDecl, SettingGroupDecl};
/// fn parameters() -> erp_sdk::Result<Vec<ParameterDecl>> {
///     Ok(vec![
///         ParameterDecl::integer("my_addon.retry_base_secs", "Retry base (s)", 5, (1, 600))?,
///         ParameterDecl::integer("my_addon.retry_cap_secs", "Retry cap (s)", 300, (1, 3_600))?,
///     ])
/// }
/// fn groups() -> erp_sdk::Result<Vec<SettingGroupDecl>> {
///     Ok(vec![SettingGroupDecl::new(
///         "my_addon.retry",
///         &["my_addon.retry_base_secs", "my_addon.retry_cap_secs"],
///         |values| match (&values["my_addon.retry_base_secs"], &values["my_addon.retry_cap_secs"]) {
///             (erp_sdk::Value::Int(base), erp_sdk::Value::Int(cap)) if base <= cap => Ok(()),
///             _ => Err(erp_sdk::Error::validation("my_addon.retry_base_secs must not exceed my_addon.retry_cap_secs.")),
///         },
///     )?])
/// }
/// erp_sdk::system_parameters!(parameters, groups);
/// ```
#[macro_export]
#[allow(
    clippy::crate_in_macro_def,
    reason = "the invoking addon crate owns the generated ADDON descriptor"
)]
macro_rules! system_parameters {
    ($factory:path) => {
        $crate::system_parameters!($factory, $crate::params::no_groups);
    };
    ($factory:path, $groups:path) => {
        #[$crate::__private::linkme::distributed_slice($crate::params::PARAMETER_CONTRIBUTIONS)]
        #[linkme(crate = $crate::__private::linkme)]
        static SYSTEM_PARAMETERS_CONTRIBUTION: $crate::params::ParameterContribution =
            $crate::params::ParameterContribution::new(&crate::ADDON, $factory, $groups);
    };
}

fn installed(registry: &Registry) -> BTreeSet<String> {
    registry
        .addons()
        .iter()
        .map(|a| a.name.to_owned())
        .collect()
}

// A contribution must come from the very descriptor the lifecycle registry installed.
fn same_descriptor(registry: &Registry, addon: &AddonDescriptor) -> Result<()> {
    match registry.addons().iter().find(|a| a.name == addon.name) {
        Some(actual) if actual.depends == addon.depends => Ok(()),
        _ => Err(Error::programming(format!(
            "The declarations of addon {} do not come from its installed descriptor.",
            addon.name
        ))),
    }
}

/// The parameter registry of `env`'s installed addons, checked as at registry publication: one
/// key space with the declared settings and Settings page items, keys inside their addon's
/// prefix, groups complete and accepting their defaults.
/// # Errors
/// The refusals of [`ParameterRegistry::build`]; a factory's error; `Programming` for a
/// contribution that does not come from the installed descriptor.
pub fn registry(env: &Env) -> Result<ParameterRegistry> {
    registry_of(env.registry())
}

/// [`registry`] for a model registry without an environment (an `#[access_domain]` override
/// reads it from its `AccessCtx`).
/// # Errors
/// As [`registry`].
pub fn registry_of(registry: &Registry) -> Result<ParameterRegistry> {
    let installed = installed(registry);
    let mut taken = BTreeSet::new();
    for entry in crate::settings::CONTRIBUTIONS {
        if installed.contains(entry.addon.name) {
            same_descriptor(registry, entry.addon)?;
            taken.extend((entry.declarations)()?.iter().map(|d| d.key().to_owned()));
        }
    }
    for entry in crate::settings::PANEL_CONTRIBUTIONS {
        if installed.contains(entry.addon.name) {
            same_descriptor(registry, entry.addon)?;
            taken.extend((entry.declarations)()?.iter().map(|d| d.key().to_owned()));
        }
    }
    let mut contributions: ParameterContributions = Vec::new();
    for entry in PARAMETER_CONTRIBUTIONS {
        if installed.contains(entry.addon.name) {
            same_descriptor(registry, entry.addon)?;
            contributions.push((
                entry.addon.name.to_owned(),
                (entry.declarations)()?,
                (entry.groups)()?,
            ));
        }
    }
    ParameterRegistry::build(contributions, &installed, taken)
}

// Whether `target` is `source` or one of its transitive dependencies among the installed addons.
fn depends_on(env: &Env, source: &str, target: &str) -> bool {
    let addons = env.registry().addons();
    let mut pending = vec![source];
    let mut seen = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if name == target {
            return true;
        }
        if seen.insert(name)
            && let Some(addon) = addons.iter().find(|a| a.name == name)
        {
            pending.extend(addon.depends.iter().copied());
        }
    }
    false
}

/// The declared value of `key` as `addon` reads it: elevated (framework code reads its own
/// configuration), committed state of this transaction after flushing pending parameter writes.
async fn read(tx: &mut Transaction, addon: &AddonDescriptor, key: &str) -> Result<Value> {
    let env = tx.env();
    let registry = registry(&env)?;
    let (Some(decl), Some(owner)) = (registry.get(key), registry.owner(key)) else {
        return Err(Error::programming(format!(
            "Parameter {key} is not declared by an installed addon."
        )));
    };
    if !installed(env.registry()).contains(addon.name) || !depends_on(&env, addon.name, owner) {
        return Err(Error::programming(format!(
            "Addon {} reads parameter {key} of {owner}, which is outside its dependencies.",
            addon.name
        )));
    }
    let model = env
        .registry()
        .model_id("ir.config_parameter")
        .ok_or_else(|| {
            Error::programming(
                "Declared parameters need the model ir.config_parameter (addon base).",
            )
        })?;
    tx.flush_model(model).await?;
    let rows = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key = $1",
            &[&key],
        )
        .await?;
    let text: Option<String> = match rows.first() {
        Some(row) => row
            .try_get(0)
            .map_err(|e| Error::db(format!("ir_config_parameter.value of {key}: {e}")))?,
        None => None,
    };
    decl.read_text(text.as_deref())
}

fn wrong_kind(key: &str, wanted: &str) -> Error {
    Error::programming(format!("Parameter {key} is not declared as {wanted}."))
}

/// The whole number of the declared parameter `key`, as the addon `addon` (its `crate::ADDON`)
/// reads it; a missing or empty row is the declared default.
/// # Errors
/// `Validation` `Stored parameter <key> is invalid.` for stored text outside the declaration;
/// `Programming` for an undeclared key, a key outside `addon`'s dependency closure, or a key not
/// declared as an integer; database errors.
pub async fn get_int(tx: &mut Transaction, addon: &AddonDescriptor, key: &str) -> Result<i64> {
    match read(tx, addon, key).await? {
        Value::Int(value) => Ok(value),
        _ => Err(wrong_kind(key, "an integer")),
    }
}

/// The number of the declared parameter `key`; see [`get_int`].
/// # Errors
/// As [`get_int`], for a key not declared as a float.
pub async fn get_float(tx: &mut Transaction, addon: &AddonDescriptor, key: &str) -> Result<f64> {
    match read(tx, addon, key).await? {
        Value::Float(value) => Ok(value),
        _ => Err(wrong_kind(key, "a float")),
    }
}

/// The flag of the declared parameter `key`; see [`get_int`].
/// # Errors
/// As [`get_int`], for a key not declared as a boolean.
pub async fn get_bool(tx: &mut Transaction, addon: &AddonDescriptor, key: &str) -> Result<bool> {
    match read(tx, addon, key).await? {
        Value::Bool(value) => Ok(value),
        _ => Err(wrong_kind(key, "a boolean")),
    }
}

/// The text (or selection key) of the declared parameter `key`; see [`get_int`].
/// # Errors
/// As [`get_int`], for a key not declared as text or a selection.
pub async fn get_str(tx: &mut Transaction, addon: &AddonDescriptor, key: &str) -> Result<String> {
    match read(tx, addon, key).await? {
        Value::Str(value) => Ok(value.to_string()),
        _ => Err(wrong_kind(key, "text or a selection")),
    }
}
