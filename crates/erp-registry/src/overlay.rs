//! Per-database overlay: declarations that belong to one database rather than to
//! its addon set — custom (`x_`) models and fields (`ir.model`/`ir.model.fields` with
//! `state = 'manual'`, MODEL §4.12) and models of addons stored in the database.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, OnceLock};

use erp_fields::{Error, Result};

use crate::decl::{ModelDecl, ModelKind};
use crate::ident::{MAX_IDENTIFIER_LEN, is_valid_pg_name};
use crate::owned::{FieldDef, InternAccount, MAX_TEXT_BYTES, ModelDef, check_model_name};

/// Most custom models one overlay may hold.
pub const MAX_MANUAL_MODELS: usize = 10_000;
/// Most custom fields one overlay may hold.
pub const MAX_MANUAL_FIELDS: usize = 100_000;
/// Most addon model definitions one overlay may hold.
pub const MAX_ADDON_DEFS: usize = 10_000;
/// Custom fields of one model per interned declaration (see `Overlay::manual_decls`).
const MANUAL_FIELDS_PER_DECL: usize = 32;

/// The pseudo-addon of custom declarations. Not a valid module name, so no addon can take it; it
/// never appears in [`crate::Registry::addons`] nor in [`crate::FieldSpec::modules`].
pub const MANUAL_ADDON: &str = "(manual)";

/// A custom model (`ir.model` with `state = 'manual'`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ManualModel {
    /// Model name; must start with `x_`.
    pub name: String,
    /// `_description` (the `ir.model` `name`).
    pub description: Option<String>,
    /// A transient model.
    pub transient: bool,
    /// `_order`.
    pub order: Option<String>,
}

impl ManualModel {
    /// A regular custom model.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            transient: false,
            order: None,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Make the model transient (a wizard).
    #[must_use]
    pub fn transient(mut self, on: bool) -> Self {
        self.transient = on;
        self
    }

    /// Set `_order`.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }
}

/// Declarations specific to one database, composed on top of its addon set.
///
/// Two overlays are equal when they hold the same declarations in the same order; the registry
/// cache shares one registry between databases with equal addon sets and overlays. The order is
/// part of the content on purpose (it is the field order of a model), so loaders add custom
/// models and fields in `ir.model`/`ir.model.fields` id order and equal databases build equal
/// overlays.
///
/// Cloning is cheap (the declarations sit behind an `Arc`, copied on the next change), and the
/// content hash is computed once per content. New bytes interned for the overlay's declarations
/// are charged to its [`InternAccount`] ([`Overlay::with_account`]; [`crate::Databases`] attaches
/// each database's own account).
///
/// ```
/// use erp_registry::{FieldDef, KindDef, ManualModel, Overlay, RegistryBuilder};
///
/// let mut overlay = Overlay::new();
/// overlay.add_manual_model(ManualModel::new("x_bananas").description("Bananas"))?;
/// overlay.add_manual_field(
///     "x_bananas",
///     FieldDef::new("x_name", KindDef::Char { size: None }).string("Name"),
/// )?;
/// let err = overlay
///     .add_manual_field("x_bananas", FieldDef::new("ripe", KindDef::Boolean))
///     .unwrap_err();
/// assert_eq!(
///     err.message(),
///     "Custom fields must have a name that starts with 'x_'! (got 'ripe' on model 'x_bananas')"
/// );
///
/// let registry = RegistryBuilder::new([]).linked(false).overlay(&overlay).build()?;
/// let bananas = registry.model("x_bananas").expect("custom model");
/// assert!(bananas.custom && bananas.field("x_name").is_some_and(|f| f.manual));
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    content: Arc<Content>,
    account: Arc<InternAccount>,
}

#[derive(Debug, Clone, Default)]
struct Content {
    addon_defs: Vec<ModelDef>,
    manual_models: Vec<ManualModel>,
    manual_fields: Vec<(String, FieldDef)>,
    /// Names in `manual_models`, for the uniqueness check.
    model_names: HashSet<String>,
    /// Table of each custom model → model name, for the table uniqueness check.
    model_tables: HashMap<String, String>,
    /// `(model, field)` pairs in `manual_fields`, for the uniqueness check.
    field_keys: HashSet<(String, String)>,
    /// Hash of the declarations, computed on first use; reset by every change.
    hash: OnceLock<u64>,
}

impl Content {
    fn content_hash(&self) -> u64 {
        *self.hash.get_or_init(|| {
            // a fixed-key hasher: the value only has to be stable for this content in this
            // process; the cache mixes it with its own random keys
            let mut hasher = DefaultHasher::new();
            self.addon_defs.hash(&mut hasher);
            self.manual_models.hash(&mut hasher);
            self.manual_fields.hash(&mut hasher);
            hasher.finish()
        })
    }
}

// Equality and hashing look at the declarations only (not the account); the name indexes are
// derived from them.
impl PartialEq for Overlay {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (&*self.content, &*other.content);
        Arc::ptr_eq(&self.content, &other.content)
            || (a.content_hash() == b.content_hash()
                && a.addon_defs == b.addon_defs
                && a.manual_models == b.manual_models
                && a.manual_fields == b.manual_fields)
    }
}

impl Eq for Overlay {}

impl Hash for Overlay {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.content.content_hash());
    }
}

impl Overlay {
    /// An empty overlay, with a fresh [`InternAccount`] of the default limit.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same declarations, charging new interned bytes to `account` (one account per
    /// database for the life of the process, so replacing the overlay does not reset its use).
    #[must_use]
    pub fn with_account(mut self, account: Arc<InternAccount>) -> Self {
        self.account = account;
        self
    }

    /// The account new interned bytes are charged to.
    pub fn account(&self) -> &Arc<InternAccount> {
        &self.account
    }

    /// True when the overlay declares nothing.
    pub fn is_empty(&self) -> bool {
        let c = &*self.content;
        c.addon_defs.is_empty() && c.manual_models.is_empty() && c.manual_fields.is_empty()
    }

    /// The declarations, for a change: copied first when another clone shares them.
    fn content_mut(&mut self) -> &mut Content {
        let content = Arc::make_mut(&mut self.content);
        content.hash = OnceLock::new();
        content
    }

    /// Add model declarations of an addon stored in the database (e.g. `models/*.toml` of a
    /// package). Their addon must be in the registry's addon set when the registry is built.
    ///
    /// # Errors
    ///
    /// `Programming` when a definition names [`MANUAL_ADDON`] as its addon or is marked custom;
    /// `Validation` when it breaks [`ModelDef::check_limits`] or the overlay would hold more than
    /// [`MAX_ADDON_DEFS`]. Nothing is added when any definition is refused.
    pub fn add_addon_defs(&mut self, defs: impl IntoIterator<Item = ModelDef>) -> Result<()> {
        let mut accepted = Vec::new();
        for def in defs {
            if def.addon == MANUAL_ADDON {
                return Err(Error::programming(format!(
                    "model definition '{}' names the custom pseudo-addon '{MANUAL_ADDON}'; add \
                     custom models with Overlay::add_manual_model",
                    def.name
                )));
            }
            if def.is_custom() {
                return Err(Error::programming(format!(
                    "model definition '{}' of addon '{}' is marked custom; add custom models \
                     with Overlay::add_manual_model",
                    def.name, def.addon
                )));
            }
            def.check_limits()?;
            let count = self.content.addon_defs.len().saturating_add(accepted.len());
            if count >= MAX_ADDON_DEFS {
                return Err(Error::validation(format!(
                    "an overlay holds at most {MAX_ADDON_DEFS} addon model definitions"
                )));
            }
            accepted.push(def);
        }
        self.content_mut().addon_defs.extend(accepted);
        Ok(())
    }

    /// Add a custom model.
    ///
    /// # Errors
    ///
    /// `Validation` for a bad or duplicate name, text attributes longer than
    /// [`MAX_TEXT_BYTES`], or more than [`MAX_MANUAL_MODELS`] custom models.
    pub fn add_manual_model(&mut self, model: ManualModel) -> Result<()> {
        if !model.name.starts_with("x_") {
            return Err(Error::validation("The model name must start with 'x_'."));
        }
        check_model_name(&model.name)?;
        for (attr, text) in [
            ("description", model.description.as_deref()),
            ("order", model.order.as_deref()),
        ] {
            if text.is_some_and(|t| t.len() > MAX_TEXT_BYTES) {
                return Err(Error::validation(format!(
                    "custom model '{}': `{attr}` is longer than {MAX_TEXT_BYTES} bytes",
                    model.name
                )));
            }
        }
        let content = &*self.content;
        if content.manual_models.len() >= MAX_MANUAL_MODELS {
            return Err(Error::validation(format!(
                "an overlay holds at most {MAX_MANUAL_MODELS} custom models"
            )));
        }
        if content.model_names.contains(&model.name) {
            return Err(Error::validation(format!(
                "Each model must have a unique name. (got '{}' twice)",
                model.name
            )));
        }
        let table = model.name.replace('.', "_");
        if let Some(other) = content.model_tables.get(&table) {
            return Err(Error::validation(format!(
                "custom models '{other}' and '{}' would share the table '{table}'",
                model.name
            )));
        }
        let content = self.content_mut();
        content.model_tables.insert(table, model.name.clone());
        content.model_names.insert(model.name.clone());
        content.manual_models.push(model);
        Ok(())
    }

    /// Add a custom field to `model` (an addon model or a custom one). Its name must start with
    /// `x_` and be a PostgreSQL name of at most 63 bytes; `(model, name)` is unique. Whether the
    /// model exists, and the field's relational targets, are checked when the registry is built.
    ///
    /// # Errors
    pub fn add_manual_field(&mut self, model: impl Into<String>, field: FieldDef) -> Result<()> {
        let model = model.into();
        check_model_name(&model)?;
        if !field.name.starts_with("x_") {
            return Err(Error::validation(format!(
                "Custom fields must have a name that starts with 'x_'! (got '{}' on model \
                 '{model}')",
                field.name
            )));
        }
        if !is_valid_pg_name(&field.name) || field.name.len() > MAX_IDENTIFIER_LEN {
            return Err(Error::validation(format!(
                "Field names can only contain characters, digits and underscores (up to 63). \
                 (got '{}' on model '{model}')",
                field.name
            )));
        }
        let key = (model.clone(), field.name.clone());
        if self.content.field_keys.contains(&key) {
            return Err(Error::validation(format!(
                "Field names must be unique per model. (got '{}' twice on model '{model}')",
                field.name
            )));
        }
        if self.content.manual_fields.len() >= MAX_MANUAL_FIELDS {
            return Err(Error::validation(format!(
                "an overlay holds at most {MAX_MANUAL_FIELDS} custom fields"
            )));
        }
        // the size limits of a single field (labels, selection, …)
        ModelDef::extension(MANUAL_ADDON, model.clone())
            .field(field.clone())
            .check_limits()?;
        let content = self.content_mut();
        content.field_keys.insert(key);
        content.manual_fields.push((model, field));
        Ok(())
    }

    /// Number of custom fields.
    pub(crate) fn manual_field_count(&self) -> usize {
        self.content.manual_fields.len()
    }

    /// `(model, field)` of each custom field, in insertion order.
    pub(crate) fn manual_field_names(&self) -> impl Iterator<Item = (&str, &str)> {
        self.content
            .manual_fields
            .iter()
            .map(|(model, field)| (model.as_str(), field.name.as_str()))
    }

    /// The same overlay (and account) without the custom fields `drop(model, field)` selects
    /// (a registry build dropping broken custom fields, [`crate::ManualFieldPolicy::DropBroken`]).
    pub(crate) fn without_manual_fields(&self, drop: impl Fn(&str, &str) -> bool) -> Self {
        let mut content = (*self.content).clone();
        content
            .manual_fields
            .retain(|(model, field)| !drop(model, &field.name));
        content
            .field_keys
            .retain(|(model, field)| !drop(model, field));
        content.hash = OnceLock::new();
        Self {
            content: Arc::new(content),
            account: Arc::clone(&self.account),
        }
    }

    /// Declarations of addons in the set, interned.
    pub(crate) fn addon_decls(&self) -> Result<Vec<&'static ModelDecl>> {
        self.content
            .addon_defs
            .iter()
            .map(|def| def.intern(Some(&self.account)))
            .collect()
    }

    /// Custom declarations, interned: the custom models in insertion order, then each model's
    /// custom fields in insertion order (models in order of their first field), in extensions of
    /// at most [`MANUAL_FIELDS_PER_DECL`] fields. Chunking bounds what one more field re-interns
    /// (the last chunk only) while keeping the model's method resolution order short.
    pub(crate) fn manual_decls(&self) -> Result<Vec<&'static ModelDecl>> {
        let account = Some(&*self.account);
        let mut decls: Vec<&'static ModelDecl> = self
            .content
            .manual_models
            .iter()
            .map(|m| {
                let mut def = ModelDef::new(MANUAL_ADDON, m.name.clone());
                def.custom = true;
                def.kind = Some(if m.transient {
                    ModelKind::Transient
                } else {
                    ModelKind::Regular
                });
                def.description.clone_from(&m.description);
                def.order.clone_from(&m.order);
                def.intern(account)
            })
            .collect::<Result<_>>()?;
        let mut order: Vec<&str> = Vec::new();
        let mut by_model: HashMap<&str, Vec<&FieldDef>> = HashMap::default();
        for (model, field) in &self.content.manual_fields {
            by_model
                .entry(model.as_str())
                .or_insert_with(|| {
                    order.push(model.as_str());
                    Vec::new()
                })
                .push(field);
        }
        for model in order {
            let fields = by_model.remove(model).ok_or_else(|| {
                Error::programming(format!(
                    "custom fields of model '{model}' were grouped twice (overlay invariant)"
                ))
            })?;
            for chunk in fields.chunks(MANUAL_FIELDS_PER_DECL) {
                let mut ext = ModelDef::extension(MANUAL_ADDON, model);
                ext.fields = chunk.iter().map(|f| (*f).clone()).collect();
                decls.push(ext.intern(account)?);
            }
        }
        Ok(decls)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use crate::owned::budget_lock::shared;

    /// Review item 10: a definition marked custom is refused when it is added, not only when the
    /// registry is built; nothing of the batch is added.
    #[test]
    fn custom_definitions_are_refused_on_insertion() {
        let _budget = shared();
        let mut overlay = Overlay::new();
        let mut fake = ModelDef::new("ov_partners", "x_fake");
        fake.custom = true;
        let err = overlay
            .add_addon_defs([ModelDef::new("ov_partners", "ov.fine"), fake])
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "model definition 'x_fake' of addon 'ov_partners' is marked custom; add custom models \
             with Overlay::add_manual_model"
        );
        assert!(overlay.is_empty());
    }

    /// Review item 6: clones share the declarations until one changes; the cached hash follows
    /// the content.
    #[test]
    fn clones_share_until_changed() {
        let _budget = shared();
        let mut a = Overlay::new();
        a.add_manual_model(ManualModel::new("x_a")).unwrap();
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.content, &b.content));
        let hash = a.content.content_hash();
        a.add_manual_model(ManualModel::new("x_b")).unwrap();
        assert!(!Arc::ptr_eq(&a.content, &b.content));
        assert_ne!(a.content.content_hash(), hash);
        assert_eq!(b.content.content_hash(), hash);
        assert_ne!(a, b);
        let mut c = Overlay::new();
        c.add_manual_model(ManualModel::new("x_a")).unwrap();
        c.add_manual_model(ManualModel::new("x_b")).unwrap();
        assert_eq!(a, c, "equal content, separate allocations");
    }
}
