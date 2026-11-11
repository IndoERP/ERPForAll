//! Broken custom fields (amending).
//!
//! Here the
//! default stays strict (the build fails); [`ManualFieldPolicy::DropBroken`] leaves such fields out
//! and lists them in [`crate::Registry::dropped_fields`], each with a [`DropReason`] and the error
//! that refused it.

use erp_fields::{Error, FieldType, Result};
use rustc_hash::FxHashMap;

use crate::decl::ModelDecl;
use crate::overlay::MANUAL_ADDON;
use crate::spec::{FieldSpec, ModelId, ModelSpec};

/// What a registry build does with a custom field it cannot set up.
///
/// ```
/// use erp_registry::{FieldDef, KindDef, ManualFieldPolicy, Overlay, RegistryBuilder};
///
/// let mut overlay = Overlay::new();
/// overlay.add_manual_field(
///     "res.users",
///     FieldDef::new("x_tag_id", KindDef::Many2one { comodel: "x_tag".into(), ondelete: None }),
/// )?;
/// // strict (the default): the whole build fails
/// let err = RegistryBuilder::new([]).linked(false).overlay(&overlay).build().unwrap_err();
/// assert_eq!(err.message(), "Field res.users.x_tag_id refers to unknown model 'x_tag'");
/// // dropping: the field is left out and reported
/// let registry = RegistryBuilder::new([])
///     .linked(false)
///     .overlay(&overlay)
///     .manual_fields(ManualFieldPolicy::DropBroken)
///     .build()?;
/// let dropped = &registry.dropped_fields()[0];
/// assert_eq!((dropped.model(), dropped.field()), ("res.users", "x_tag_id"));
/// assert_eq!(dropped.error().message(), err.message());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ManualFieldPolicy {
    /// A broken custom field fails the build.
    #[default]
    Strict,
    /// A custom field the build refuses for a reason of its own is left out and reported in
    /// [`crate::Registry::dropped_fields`]; every other error still fails the build.
    DropBroken,
}

/// Why a custom field was left out of a registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DropReason {
    /// The model the field extends is not in the registry (its addon was removed, or a newer
    /// version of it no longer declares the model).
    UnknownModel,
    /// Addons declare a field of the same name on the model (e.g.
    DeclaredByAddon {
        /// The addons declaring it, in override order.
        addons: Vec<&'static str>,
    },
    /// The field's comodel is not in the registry.
    UnknownComodel {
        /// The missing model.
        comodel: String,
    },
    /// The field's related path does not resolve, or its target has another type.
    Related {
        /// The path as declared.
        path: String,
    },
    /// Another check of the registry refused the field (one2many inverse, many2one or many2many
    /// rules, field parameters); [`DroppedField::error`] says which.
    Refused,
}

/// A custom field left out of a registry by [`ManualFieldPolicy::DropBroken`].
#[derive(Debug)]
pub struct DroppedField {
    model: &'static str,
    field: &'static str,
    reason: DropReason,
    error: Error,
}

impl DroppedField {
    /// The model the custom field was declared on (models inheriting it under another name
    /// lose it too).
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The custom field's name.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Why it was dropped.
    pub fn reason(&self) -> &DropReason {
        &self.reason
    }

    /// The error that refused it (what a strict build reports for this field).
    pub fn error(&self) -> &Error {
        &self.error
    }
}

/// True for a field only an overlay declares: custom, extended by no addon, and not the
/// inherited copy of a parent's field (that copy goes with its parent field).
pub(crate) fn is_pure_custom(field: &FieldSpec) -> bool {
    field.manual && field.modules.is_empty() && field.inherited.is_none()
}

/// The model whose custom declaration (in `mro`, newest first) declares `field`, when `field` is a
/// pure custom field; `None` otherwise.
pub(crate) fn declared_on(mro: &[&'static ModelDecl], field: &FieldSpec) -> Option<&'static str> {
    if !is_pure_custom(field) {
        return None;
    }
    mro.iter()
        .find(|d| d.addon == MANUAL_ADDON && d.fields.iter().any(|f| f.name == field.name))
        .map(|d| d.name)
}

/// Collects the custom fields a composition round refuses (`DropBroken`), or turns each refusal
/// into the build's error (`Strict`).
#[derive(Debug)]
pub(crate) struct Sink {
    policy: ManualFieldPolicy,
    drops: Vec<DroppedField>,
    /// An error the build must return even when the round also dropped fields. A round with
    /// drops is composed again without them, which discards the round's error; that is right for
    /// errors a dropped field causes (they go with it), wrong for one that dropping would hide.
    fatal: Option<Error>,
}

impl Sink {
    pub(crate) fn new(policy: ManualFieldPolicy) -> Self {
        Self {
            policy,
            drops: Vec::new(),
            fatal: None,
        }
    }

    /// Record `error` as the build's error whatever else the round drops (the first one wins),
    /// and return it for the composer to stop with.
    pub(crate) fn fatal(&mut self, error: Error) -> Error {
        // composer errors carry a kind and a message only, so the copy is the same error
        let copy = Error::new(error.kind(), error.message());
        if self.fatal.is_none() {
            self.fatal = Some(error);
        }
        copy
    }

    /// `error` refuses field `field` of `model`. `Ok` when the field is dropped (only a pure
    /// custom field under `DropBroken`); the caller then skips the field.
    ///
    /// # Errors
    ///
    /// `error` itself in every other case.
    pub(crate) fn refuse(
        &mut self,
        custom: bool,
        model: &'static str,
        field: &'static str,
        reason: DropReason,
        error: Error,
    ) -> Result<()> {
        if custom && self.policy == ManualFieldPolicy::DropBroken {
            self.drops.push(DroppedField {
                model,
                field,
                reason,
                error,
            });
            Ok(())
        } else {
            Err(error)
        }
    }

    /// [`Sink::refuse`] for a composed field of a model with method resolution order `mro`. A
    /// custom field is reported on the model its overlay declared it on, which differs from the
    /// composed model when that model inherits it under another name.
    pub(crate) fn field(
        &mut self,
        mro: &[&'static ModelDecl],
        field: &FieldSpec,
        reason: DropReason,
        error: Error,
    ) -> Result<()> {
        match declared_on(mro, field) {
            Some(model) => self.refuse(true, model, field.name, reason, error),
            None => Err(error),
        }
    }

    /// The drops of the round, or its fatal error.
    ///
    /// # Errors
    ///
    /// The error recorded with [`Sink::fatal`].
    pub(crate) fn into_drops(self) -> Result<Vec<DroppedField>> {
        match self.fatal {
            Some(error) => Err(error),
            None => Ok(self.drops),
        }
    }
}

/// Native related fields are checked
/// by `erp-orm` when it builds its compute graph.
pub(crate) fn check_custom_related(
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    sink: &mut Sink,
) -> Result<()> {
    for model in models {
        for field in &model.fields {
            let Some(path) = field.related else {
                continue;
            };
            if !is_pure_custom(field) {
                continue;
            }
            if let Err(error) = resolve_related(models, by_name, model, field, path) {
                sink.field(
                    &model.mro,
                    field,
                    DropReason::Related {
                        path: path.to_owned(),
                    },
                    error,
                )?;
            }
        }
    }
    Ok(())
}

fn resolve_related(
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    model: &ModelSpec,
    field: &FieldSpec,
    path: &str,
) -> Result<()> {
    let mut current = model;
    let mut segments = path.split('.').peekable();
    while let Some(name) = segments.next() {
        let Some(step) = current.field(name) else {
            return Err(Error::programming(format!(
                "Field {}.{}: related path '{path}': field '{name}' does not exist on model '{}'",
                model.name, field.name, current.name
            )));
        };
        if segments.peek().is_none() {
            if step.ty.type_name() != field.ty.type_name() {
                return Err(Error::programming(format!(
                    "Type of related field {}.{} ({}) is inconsistent with {}.{} ({})",
                    model.name,
                    field.name,
                    field.ty.type_name(),
                    current.name,
                    step.name,
                    step.ty.type_name()
                )));
            }
            return Ok(());
        }
        let comodel = match &step.ty {
            FieldType::Many2one { comodel, .. }
            | FieldType::One2many { comodel, .. }
            | FieldType::Many2many { comodel, .. } => comodel.as_str(),
            _ => {
                return Err(Error::programming(format!(
                    "Field {}.{}: related path '{path}' goes through {}.{}, which is not a \
                     many2one, one2many or many2many field",
                    model.name, field.name, current.name, step.name
                )));
            }
        };
        let Some(id) = by_name.get(comodel) else {
            return Err(Error::programming(format!(
                "Field {}.{}: related path '{path}' goes through {}.{}, whose comodel \
                 '{comodel}' is not in the registry",
                model.name, field.name, current.name, step.name
            )));
        };
        current = &models[id.index()];
    }
    // `split` yields at least one segment, and the last one returns above
    Err(Error::programming(format!(
        "Field {}.{}: related path '{path}' is empty",
        model.name, field.name
    )))
}
