//! The mandatory H18 view-type declaration.
//!
//! A [`ViewTypeDecl`] has private fields and one constructor, [`ViewTypeDeclBuilder::finish`],
//! which refuses a declaration that lacks its schema, validator, builder or metadata field set,
//! names an unknown data method, or declares wire major 0. The reference visitor is derived from
//! the declared schema, so no callback can omit a slot. The validator and builder are typed
//! against the checked host input ([`CheckedViewInput`]); the builder returns a value that the
//! framework wraps with [`DescriptionBuilder::build`], which enforces the declared schema and the
//! caller's budget identity.
use crate::registry::{is_valid_ident, is_valid_view_type_key, reserved_owner};
use crate::{
    CheckedViewInput, DescriptionBudget, DescriptionBuilder, DescriptionPayload, DescriptionValue,
    DescriptionValueBuilder, MetadataFieldSet, ReferenceVisitor, ViewError, ViewResult, ViewSchema,
};

/// Validate a checked view input against the declared type, without building anything.
pub type ValidateView = fn(&CheckedViewInput<'_>, &DescriptionBudget) -> ViewResult<()>;
/// Build the description value for a checked input. The cursor is bound to the declared schema and
/// the request budget; only a value made from it can be returned.
pub type BuildView =
    fn(&CheckedViewInput<'_>, DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue>;
/// A native addon's declaration, produced on demand because validation is not `const`.
pub type ViewTypeDeclFn = fn() -> ViewResult<ViewTypeDecl>;

const MAX_METHODS: usize = 64;

pub(crate) fn shown(text: &str) -> String {
    format!("{:?}", text.chars().take(64).collect::<String>())
}
/// A located declaration refusal.
pub(crate) fn located(reason: &str, addon: &str, key: &str) -> ViewError {
    ViewError::InvalidDescription(format!(
        "View-type declaration {reason} (view type {} of addon {})",
        shown(key),
        shown(addon)
    ))
}

/// A validated H18 view-type registration.
#[derive(Debug, Clone)]
pub struct ViewTypeDecl {
    addon: &'static str,
    key: &'static str,
    wire_major: u8,
    renderer: &'static str,
    visitor: ReferenceVisitor,
    validator: ValidateView,
    builder: BuildView,
    metadata_fields: MetadataFieldSet,
    methods: &'static [&'static str],
}
impl ViewTypeDecl {
    /// Start a declaration for `key` owned by `addon`.
    pub fn builder(addon: &'static str, key: &'static str) -> ViewTypeDeclBuilder {
        ViewTypeDeclBuilder {
            addon,
            key,
            wire_major: None,
            renderer: None,
            schema: None,
            validator: None,
            builder: None,
            metadata_fields: None,
            methods: None,
        }
    }
    /// Owning addon name.
    pub fn addon(&self) -> &'static str {
        self.addon
    }
    /// Wire view-type key.
    pub fn key(&self) -> &'static str {
        self.key
    }
    /// Supported wire major version (at least 1).
    pub fn wire_major(&self) -> u8 {
        self.wire_major
    }
    /// Client renderer key.
    pub fn renderer(&self) -> &'static str {
        self.renderer
    }
    /// The declared closed description schema.
    pub fn schema(&self) -> &ViewSchema {
        self.visitor.schema()
    }
    /// The reference visitor derived from the declared schema.
    pub fn visitor(&self) -> &ReferenceVisitor {
        &self.visitor
    }
    /// The declared metadata field set.
    pub fn metadata_fields(&self) -> &MetadataFieldSet {
        &self.metadata_fields
    }
    /// Declared data-method names; a name grants no authority.
    pub fn methods(&self) -> &'static [&'static str] {
        self.methods
    }
    /// Run the declared validator over a checked input.
    pub fn validate(
        &self,
        input: &CheckedViewInput<'_>,
        budget: &DescriptionBudget,
    ) -> ViewResult<()> {
        budget.check()?;
        (self.validator)(input, budget)
    }
    /// Run the declared builder through [`DescriptionBuilder::build`] over the declared schema and
    /// the caller's budget.
    pub fn build(
        &self,
        input: &CheckedViewInput<'_>,
        budget: &DescriptionBudget,
    ) -> ViewResult<DescriptionPayload> {
        DescriptionBuilder::new(self.schema(), budget).build(|root| (self.builder)(input, root))
    }
}

/// Collects a declaration; every part is mandatory and checked by [`Self::finish`].
#[derive(Debug)]
pub struct ViewTypeDeclBuilder {
    addon: &'static str,
    key: &'static str,
    wire_major: Option<u8>,
    renderer: Option<&'static str>,
    schema: Option<ViewSchema>,
    validator: Option<ValidateView>,
    builder: Option<BuildView>,
    metadata_fields: Option<MetadataFieldSet>,
    methods: Option<&'static [&'static str]>,
}
impl ViewTypeDeclBuilder {
    /// Supported wire major version; 0 is refused at `finish`.
    pub fn wire_major(mut self, major: u8) -> Self {
        self.wire_major = Some(major);
        self
    }
    /// Client renderer key.
    pub fn renderer(mut self, renderer: &'static str) -> Self {
        self.renderer = Some(renderer);
        self
    }
    /// The closed description schema (already admitted under the registration caps).
    pub fn schema(mut self, schema: &ViewSchema) -> Self {
        self.schema = Some(schema.clone());
        self
    }
    /// The checked-input validator.
    pub fn validator(mut self, validator: ValidateView) -> Self {
        self.validator = Some(validator);
        self
    }
    /// The checked-input description builder.
    pub fn builder(mut self, builder: BuildView) -> Self {
        self.builder = Some(builder);
        self
    }
    /// The metadata field set.
    pub fn metadata_fields(mut self, fields: MetadataFieldSet) -> Self {
        self.metadata_fields = Some(fields);
        self
    }
    /// Allowed model data-method names (possibly none); a name grants no authority.
    pub fn methods(mut self, methods: &'static [&'static str]) -> Self {
        self.methods = Some(methods);
        self
    }
    /// Validate the whole declaration.
    pub fn finish(self) -> ViewResult<ViewTypeDecl> {
        let refuse = |reason: &str| located(reason, self.addon, self.key);
        if !is_valid_ident(self.addon) {
            return Err(refuse("has an invalid addon identifier"));
        }
        if !is_valid_view_type_key(self.key) {
            return Err(refuse("has an invalid key"));
        }
        let wire_major = self
            .wire_major
            .ok_or_else(|| refuse("is missing its wire major"))?;
        if wire_major == 0 {
            return Err(refuse("has unsupported wire major 0"));
        }
        let renderer = self
            .renderer
            .ok_or_else(|| refuse("is missing its renderer"))?;
        if !is_valid_view_type_key(renderer) {
            return Err(refuse("has an invalid renderer"));
        }
        let schema = self
            .schema
            .clone()
            .ok_or_else(|| refuse("is missing its schema"))?;
        let validator = self
            .validator
            .ok_or_else(|| refuse("is missing its validator"))?;
        let builder = self
            .builder
            .ok_or_else(|| refuse("is missing its builder"))?;
        let metadata_fields = self
            .metadata_fields
            .clone()
            .ok_or_else(|| refuse("is missing its metadata field set"))?;
        let methods = self
            .methods
            .ok_or_else(|| refuse("is missing its data-method list"))?;
        if methods.len() > MAX_METHODS {
            return Err(refuse("declares too many data methods"));
        }
        for (index, name) in methods.iter().enumerate() {
            if !is_valid_ident(name) {
                return Err(refuse("declares an invalid data method name"));
            }
            if methods[..index].contains(name) {
                return Err(refuse("declares a data method twice"));
            }
        }
        // admission rules.
        if let Some((prefix, _)) = self.key.split_once('.') {
            if prefix != self.addon {
                return Err(refuse(
                    "has a namespaced key whose prefix is not its owning addon",
                ));
            }
        } else {
            match reserved_owner(self.key) {
                None => {
                    return Err(refuse(
                        "has an unreserved bare key; custom view types must use <addon>.<name>",
                    ));
                }
                Some(owner) if owner != self.addon => {
                    return Err(refuse("has a bare key reserved for another owning addon"));
                }
                Some(_) => {}
            }
        }
        Ok(ViewTypeDecl {
            addon: self.addon,
            key: self.key,
            wire_major,
            renderer,
            visitor: ReferenceVisitor::new(schema),
            validator,
            builder,
            metadata_fields,
            methods,
        })
    }
}
