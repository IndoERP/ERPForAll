//! Raw panel section DTOs; structural deserialization grants no model or caller authority.
use super::{FieldRef, WireId};
use erp_expr::wire::WireDocument;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// The supported section-description version, independent of the outer wire major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[serde(try_from = "u8", into = "u8")]
pub struct PanelVersion(#[cfg_attr(feature = "schema-gen", ts(type = "1"))] u8);
impl PanelVersion {
    /// Section contract version one.
    pub const V1: Self = Self(1);
}
impl TryFrom<u8> for PanelVersion {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self::V1)
        } else {
            Err("unsupported search panel version")
        }
    }
}
impl From<PanelVersion> for u8 {
    fn from(value: PanelVersion) -> Self {
        value.0
    }
}

/// Nonnegative display limit exactly representable by the browser. Zero does not lift safety limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[serde(try_from = "u64", into = "u64")]
pub struct PanelDisplayLimit(#[cfg_attr(feature = "schema-gen", ts(type = "number"))] u64);
impl TryFrom<u64> for PanelDisplayLimit {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value <= 9_007_199_254_740_991 {
            Ok(Self(value))
        } else {
            Err("search panel display limit must be a nonnegative JSON-safe integer")
        }
    }
}
impl From<PanelDisplayLimit> for u64 {
    fn from(value: PanelDisplayLimit) -> Self {
        value.0
    }
}

/// A raw default scalar; the host overlay separately checks the section's field kind and access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[serde(untagged)]
#[non_exhaustive]
pub enum PanelValue {
    /// A declared selection key, never an opaque custom relational identity.
    Selection(String),
    /// A positive browser-safe relational identity.
    Record(WireId),
}

object!(/// A bounded domain document whose target model must match the section's actual comodel.
    PanelDomain { model: String, ast: WireDocument });
impl PanelDomain {
    /// Construct a raw descriptive domain; callers still owe grammar, reference and access checks.
    ///
    /// ```compile_fail
    /// use erp_views::wire::PanelDomain;
    /// let expr=erp_expr::parse("[]").unwrap();
    /// let ast=erp_expr::wire::WireDocument::from_expr(&expr).unwrap();
    /// PanelDomain::new("panel.tag".to_owned(), ast);
    /// ```
    pub(crate) fn new(model: String, ast: WireDocument) -> Self {
        Self { model, ast }
    }
}

/// Raw default storage. Deserialization is transport only, never authorization.
/// This type does not seal the invariant: the `PanelSection` variant fields stay public
/// and this type deserializes, so code holding `&mut PanelSection` can still replace it.
/// The invariant comes from the host sequence: final-host refusal of structural defaults,
/// then the checked overlay (`PanelCallerDefaults::checked` and `set_caller_defaults`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[non_exhaustive]
pub struct PanelDefaults(Vec<PanelValue>);
impl PanelDefaults {
    /// Whether this structural/raw default array is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Inspect defaults without exposing unchecked mutation.
    pub fn as_slice(&self) -> &[PanelValue] {
        &self.0
    }
}

/// Ordered raw category/filter declarations. Host validation and client admission are independent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema-gen", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum PanelSection {
    /// A single selected category; parent identity is returned by its authorized range RPC.
    Category {
        /// Positive description-local identity, assigned before pruning.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        id: NonZeroU32,
        /// Root-model field, independently checked by the host.
        field: FieldRef,
        /// Bounded localized display label.
        label: String,
        /// Optional bounded icon token; never HTML or a URL.
        #[serde(deserialize_with = "super::required_nullable")]
        icon: Option<String>,
        /// Requested counters before host compatibility derivation, effective counters afterward.
        enable_counters: bool,
        /// Whether the range may include values outside the current image.
        expand: bool,
        /// Display limit, independent from admission budgets.
        limit: PanelDisplayLimit,
        /// Caller overlay only; structural callbacks cannot supply defaults.
        #[cfg_attr(feature = "schema-gen", ts(type = "Array<PanelValue>"))]
        default_values: PanelDefaults,
        /// Request the relation's declared hierarchy; selection categories remain flat.
        hierarchize: bool,
    },
    /// A multi-selection filter with optional authorized comodel domain and grouping.
    Filter {
        /// Positive description-local identity, assigned before pruning.
        #[cfg_attr(feature = "schema-gen", ts(type = "number"))]
        id: NonZeroU32,
        /// Root-model field, independently checked by the host.
        field: FieldRef,
        /// Bounded localized display label.
        label: String,
        /// Optional bounded icon token; never HTML or a URL.
        #[serde(deserialize_with = "super::required_nullable")]
        icon: Option<String>,
        /// Whether counter results are requested.
        enable_counters: bool,
        /// Whether the range may include values outside the current image.
        expand: bool,
        /// Display limit, independent from admission budgets.
        limit: PanelDisplayLimit,
        /// Caller overlay only; structural callbacks cannot supply defaults.
        #[cfg_attr(feature = "schema-gen", ts(type = "Array<PanelValue>"))]
        default_values: PanelDefaults,
        /// Scoped AST; criterion fields and category variables have distinct namespaces.
        #[serde(deserialize_with = "super::required_nullable")]
        domain: Option<PanelDomain>,
        /// Checked field on the relation's actual comodel.
        #[serde(deserialize_with = "super::required_nullable")]
        group_by: Option<FieldRef>,
    },
}
impl PanelSection {
    /// Description-local identity; this is not a database ID or authorization witness.
    pub fn id(&self) -> u32 {
        match self {
            Self::Category { id, .. } | Self::Filter { id, .. } => id.get(),
        }
    }
    /// Root-model field named by this section.
    pub fn field(&self) -> &FieldRef {
        match self {
            Self::Category { field, .. } | Self::Filter { field, .. } => field,
        }
    }
    /// Caller defaults after authorization; never populated by structural callbacks.
    ///
    /// ```compile_fail
    /// use erp_views::wire::{PanelSection,PanelValue};
    /// fn overwrite(section: &mut PanelSection) {
    ///     section.defaults().push(PanelValue::Selection("unchecked".into()));
    /// }
    /// ```
    pub fn defaults(&self) -> &[PanelValue] {
        match self {
            Self::Category { default_values, .. } | Self::Filter { default_values, .. } => {
                default_values.as_slice()
            }
        }
    }

    /// Install a validated, section-bound caller overlay after access checks.
    /// # Errors
    /// A located Validation error when the value belongs to another section/model/kind.
    #[cfg(feature = "host-access")]
    pub fn set_caller_defaults(&mut self, defaults: PanelCallerDefaults) -> erp_fields::Result<()> {
        if self.id() != defaults.section_id
            || self.field().model != defaults.model
            || self.field().name != defaults.field
            || matches!(self, Self::Category { .. }) != defaults.category
        {
            return Err(erp_fields::Error::validation(format!(
                "Search panel section {} has caller defaults for another section.",
                self.id()
            )));
        }
        match self {
            Self::Category { default_values, .. } | Self::Filter { default_values, .. } => {
                *default_values = defaults.values
            }
        }
        Ok(())
    }
}

/// Validated caller defaults bound to one description-local section and registry field.
/// Private fields and no deserializer keep raw callback/transport values outside this overlay.
/// This value conveys no access authority; install it only after final section authorization.
#[cfg(feature = "host-access")]
#[derive(Debug)]
#[non_exhaustive]
pub struct PanelCallerDefaults {
    section_id: u32,
    model: &'static str,
    field: &'static str,
    category: bool,
    values: PanelDefaults,
}
#[cfg(feature = "host-access")]
impl PanelCallerDefaults {
    /// Check owned values against the section's actual registry model and field. Reserve the
    /// vector/text storage under the request budget before constructing `values`. Schema names
    /// are borrowed from the registry, so this wrapper copies no model/field strings.
    /// # Errors
    /// Located Validation errors for a wrong model/field, invalid kind/count/key or duplicate.
    pub fn checked(
        section: &PanelSection,
        model: &erp_registry::ModelSpec,
        values: Vec<PanelValue>,
    ) -> erp_fields::Result<Self> {
        use erp_fields::{Error, FieldType};
        let id = section.id();
        let invalid =
            |reason: &str| Error::validation(format!("Search panel section {id} {reason}."));
        if section.field().model != model.name {
            return Err(invalid("references an invalid root model"));
        }
        let field = model
            .field(&section.field().name)
            .ok_or_else(|| invalid("references an invalid root field"))?;
        // Empty/All is still an overlay for a supported section field, never a way to
        // mint a checked value for an unsupported type by skipping the value loop.
        match &field.ty {
            FieldType::Selection {
                values: Some(choices),
                ..
            } if !choices.is_empty() => {}
            FieldType::Many2one { .. } | FieldType::Many2many { .. } => {}
            _ => return Err(invalid("has an unsupported default field")),
        }
        let category = matches!(section, PanelSection::Category { .. });
        if values.len() > if category { 1 } else { 64 } {
            return Err(invalid("exceeds default count limit"));
        }
        for (i, value) in values.iter().enumerate() {
            match (&field.ty, value) {
                (
                    FieldType::Selection {
                        values: Some(choices),
                        ..
                    },
                    PanelValue::Selection(key),
                ) => {
                    if key.len() > 256 {
                        return Err(invalid("exceeds selection key byte limit"));
                    }
                    if key.is_empty() || !choices.iter().any(|(choice, _)| choice == key) {
                        return Err(invalid("has an invalid selection default"));
                    }
                }
                (
                    FieldType::Many2one { .. } | FieldType::Many2many { .. },
                    PanelValue::Record(_),
                ) => {}
                (FieldType::Selection { .. }, _) => {
                    return Err(invalid("has an invalid selection default"));
                }
                (FieldType::Many2one { .. } | FieldType::Many2many { .. }, _) => {
                    return Err(invalid("has an invalid relational default"));
                }
                _ => return Err(invalid("has an unsupported default field")),
            }
            if values[..i].contains(value) {
                return Err(invalid("has duplicate defaults"));
            }
        }
        Ok(Self {
            section_id: id,
            model: model.name,
            field: field.name,
            category,
            values: PanelDefaults(values),
        })
    }
}
