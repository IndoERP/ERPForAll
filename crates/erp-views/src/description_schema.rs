//! Closed payload schemas and the host's independently derived field-reference visitor.
use crate::expression_decode::{Slot, collect_value};
use crate::reference_index::Collector;
use crate::{DescriptionBudget, ExpressionKind, SchemaReferences, ViewError, ViewResult};
use serde_json::Value;
use std::collections::BTreeSet;

// Registration caps: what one declared schema may be (registration row).
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_DEPTH: usize = 64;
const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
// Payload caps: what one description may be, however small its schema (D7, description rows).
// A schema's arrays repeat their item slot, so a payload legitimately has many more nodes than the
// schema has slots; the two limits are never interchangeable.
/// Values in one walked payload, the root included.
pub(crate) const PAYLOAD_MAX_NODES: usize = 50_000;
/// Depth of one walked payload, the root at depth 1; an expression adds its logical height.
pub(crate) const PAYLOAD_MAX_DEPTH: usize = 128;

/// Where the references of a slot are resolved from. Scope names are untrusted identities for the
/// host's later registry resolution; they grant nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReferenceScope {
    /// The checked input's root model.
    Root,
    /// This explicit model, never the root.
    Model(&'static str),
}
impl ReferenceScope {
    pub(crate) fn model(&self) -> Option<&'static str> {
        match self {
            Self::Root => None,
            Self::Model(model) => Some(model),
        }
    }
    fn valid(&self) -> bool {
        self.model().is_none_or(model_reference)
    }
}

/// An untrusted field reference retaining its declared model scope for host resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum FieldReference<'a> {
    /// Resolve from the checked input's root model, following every relation segment.
    RootPath(&'a str),
    /// Resolve from this explicit model; never substitute the root model.
    ModelPath {
        /// Declared model identity, still requiring registry/access checks.
        model: &'a str,
        /// Static field/relation path.
        path: &'a str,
    },
}

/// A structural description slot. Only validated schemas can be used by a visitor.
#[derive(Debug)]
#[non_exhaustive]
pub enum SchemaNode {
    /// An object closed over these named properties.
    Object(&'static [SchemaProperty]),
    /// A bounded homogeneous array.
    Array {
        /// Element schema.
        item: &'static SchemaNode,
        /// Maximum number of elements.
        max_items: usize,
    },
    /// Text that never acts as a model reference.
    Text {
        /// Maximum UTF-8 bytes.
        max_bytes: usize,
    },
    /// A static dotted model-field path; every segment is checked by the host.
    FieldPath,
    /// A closed model/name handle with explicit model scope.
    FieldHandle,
    /// A closed `{model, ast}` domain document with criterion paths in the explicit model
    /// and RHS references in the root scope. Its model is descriptive, never an authority.
    ScopedDomain,
    /// A JSON boolean.
    Bool,
    /// A JSON signed integer.
    Integer,
    /// An admitted expression embedded as its versioned wire document. Built only through
    /// [`crate::DescriptionValueBuilder::expression`], which reserves its storage first.
    Expression {
        /// What the expression text means (domain, context or value).
        kind: ExpressionKind,
        /// Scope of its field references.
        scope: ReferenceScope,
        /// The model `parent.<path>` references resolve against; without one they refuse.
        parent_model: Option<&'static str>,
    },
    /// A group-by entry: a static field path with at most one temporal suffix (`day`, `week`,
    /// `month`, `quarter` or `year`). The reference is the path without the suffix.
    GroupBy {
        /// Scope of the field reference.
        scope: ReferenceScope,
    },
    /// An internally tagged union: a JSON object whose `discriminator` member names one declared
    /// variant, whose other members are that variant's closed object. No wildcard variant exists.
    TaggedUnion {
        /// Name of the member that carries the tag.
        discriminator: &'static str,
        /// The declared variants.
        variants: &'static [SchemaVariant],
    },
}

/// One branch of a [`SchemaNode::TaggedUnion`]: a tag and the closed object it selects.
#[derive(Debug)]
pub struct SchemaVariant {
    pub(crate) tag: &'static str,
    pub(crate) object: &'static SchemaNode,
}
impl SchemaVariant {
    /// Declare a variant. Schema admission requires `object` to be an object schema that does not
    /// use the union's discriminator as a property.
    pub const fn new(tag: &'static str, object: &'static SchemaNode) -> Self {
        Self { tag, object }
    }
}

/// One object property; optional means absent is allowed, not arbitrary or null data.
#[derive(Debug)]
pub struct SchemaProperty {
    pub(crate) name: &'static str,
    pub(crate) node: &'static SchemaNode,
    pub(crate) required: bool,
}
impl SchemaProperty {
    /// Declare a required, structurally typed property.
    pub const fn required(name: &'static str, node: &'static SchemaNode) -> Self {
        Self {
            name,
            node,
            required: true,
        }
    }
    /// Declare an optional, structurally typed property.
    pub const fn optional(name: &'static str, node: &'static SchemaNode) -> Self {
        Self {
            name,
            node,
            required: false,
        }
    }
}

/// Validated, immutable schema. Its only constructor checks version and all structural bounds.
#[derive(Debug, Clone)]
pub struct ViewSchema {
    revision: u16,
    root: &'static SchemaNode,
}
impl ViewSchema {
    /// Admit a closed schema; unsupported revision, duplicate names and unsafe bounds refuse.
    pub fn new(revision: u16, root: &'static SchemaNode) -> ViewResult<Self> {
        if revision != 1 {
            return Err(invalid(
                "Unsupported description schema revision; reload with a compatible registry",
            ));
        }
        let mut nodes = 0usize;
        let mut bytes = 0usize;
        validate_schema(root, 0, &mut nodes, &mut bytes)?;
        Ok(Self { revision, root })
    }
    pub(crate) fn root_node(&self) -> &'static SchemaNode {
        self.root
    }
    /// The validated schema contract revision.
    pub fn revision(&self) -> u16 {
        self.revision
    }
}

fn invalid(message: impl Into<String>) -> ViewError {
    ViewError::InvalidDescription(message.into())
}
fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
pub(crate) fn model_reference(model: &str) -> bool {
    !model.is_empty() && model.len() <= 4096 && !model.chars().any(char::is_control)
}
pub(crate) fn field_path(path: &str) -> bool {
    path.len() <= 4096
        && path.split('.').all(|segment| {
            segment.len() <= erp_registry::ident::MAX_IDENTIFIER_LEN
                && erp_registry::ident::is_valid_pg_name(segment)
        })
}
fn validate_schema(
    node: &SchemaNode,
    depth: usize,
    nodes: &mut usize,
    bytes: &mut usize,
) -> ViewResult<()> {
    *nodes += 1;
    *bytes += std::mem::size_of::<SchemaNode>();
    if depth > MAX_DEPTH || *nodes > MAX_SCHEMA_NODES || *bytes > MAX_SCHEMA_BYTES {
        return Err(invalid(
            "Description schema exceeds its node, depth or byte limit",
        ));
    }
    match node {
        SchemaNode::Object(properties) => {
            if properties.len() > MAX_SCHEMA_NODES {
                return Err(invalid("Description schema has too many properties"));
            }
            let mut seen = BTreeSet::new();
            for property in *properties {
                if !identifier(property.name) || !seen.insert(property.name) {
                    return Err(invalid(
                        "Description schema has an invalid or duplicate property",
                    ));
                }
                *bytes += property.name.len() + std::mem::size_of::<SchemaProperty>();
                validate_schema(property.node, depth + 1, nodes, bytes)?;
            }
        }
        SchemaNode::Array { item, max_items } => {
            if *max_items == 0 || *max_items > MAX_SCHEMA_NODES {
                return Err(invalid(
                    "Description schema requires a bounded nonempty array allowance",
                ));
            }
            validate_schema(item, depth + 1, nodes, bytes)?;
        }
        SchemaNode::Text { max_bytes } if *max_bytes == 0 || *max_bytes > MAX_TEXT_BYTES => {
            return Err(invalid("Description schema text allowance is invalid"));
        }
        SchemaNode::Expression {
            scope,
            parent_model,
            ..
        } => {
            if !scope.valid() || parent_model.is_some_and(|model| !model_reference(model)) {
                return Err(invalid("Description schema has an invalid model scope"));
            }
        }
        SchemaNode::GroupBy { scope } => {
            if !scope.valid() {
                return Err(invalid("Description schema has an invalid model scope"));
            }
        }
        SchemaNode::TaggedUnion {
            discriminator,
            variants,
        } => {
            if !identifier(discriminator) {
                return Err(invalid(
                    "Description schema tagged union has an invalid discriminator",
                ));
            }
            if variants.is_empty() {
                return Err(invalid(
                    "Description schema tagged union requires at least one variant",
                ));
            }
            if variants.len() > MAX_SCHEMA_NODES {
                return Err(invalid(
                    "Description schema tagged union has too many variants",
                ));
            }
            let mut seen = BTreeSet::new();
            for variant in *variants {
                if !identifier(variant.tag) || !seen.insert(variant.tag) {
                    return Err(invalid(
                        "Description schema tagged union has an invalid or duplicate variant tag",
                    ));
                }
                let SchemaNode::Object(properties) = variant.object else {
                    return Err(invalid(
                        "Description schema tagged union variant must be an object",
                    ));
                };
                if properties
                    .iter()
                    .any(|property| property.name == *discriminator)
                {
                    return Err(invalid(
                        "Description schema variant property collides with its discriminator",
                    ));
                }
                *bytes += variant.tag.len() + std::mem::size_of::<SchemaVariant>();
                validate_schema(variant.object, depth + 1, nodes, bytes)?;
            }
        }
        SchemaNode::Text { .. }
        | SchemaNode::FieldPath
        | SchemaNode::FieldHandle
        | SchemaNode::ScopedDomain
        | SchemaNode::Bool
        | SchemaNode::Integer => {}
    }
    if *bytes > MAX_SCHEMA_BYTES {
        return Err(invalid("Description schema exceeds its byte limit"));
    }
    Ok(())
}

/// A visitor derived only from a validated schema; no addon callback can omit a slot.
#[derive(Debug, Clone)]
pub struct ReferenceVisitor {
    schema: ViewSchema,
}
impl ReferenceVisitor {
    pub(crate) fn schema(&self) -> &ViewSchema {
        &self.schema
    }
    /// Derive reference traversal from the complete closed schema.
    pub fn new(schema: ViewSchema) -> Self {
        Self { schema }
    }
    /// Validate the complete payload and retain all its declared references under one budget.
    ///
    /// Input text stays borrowed; paths derived from expressions are owned by the returned index,
    /// which retains its construction reservation. Expression slots are decoded and walked under
    /// the request's shared expression work and byte allowances. This is a structural result,
    /// never a caller-authorization witness.
    pub fn references<'a>(
        &self,
        payload: &'a Value,
        budget: &DescriptionBudget,
    ) -> ViewResult<SchemaReferences<'a>> {
        budget.check()?;
        let mut walk = Walk {
            budget,
            visited: 0,
            collector: Collector::new(budget)?,
        };
        walk.node(self.schema.root, payload, 1)?;
        walk.collector.finish()
    }
}

struct Walk<'a, 'b> {
    budget: &'b DescriptionBudget,
    visited: usize,
    collector: Collector<'a>,
}
impl<'a> Walk<'a, '_> {
    fn enter(&mut self, depth: usize) -> ViewResult<()> {
        self.visited += 1;
        if self.visited > PAYLOAD_MAX_NODES {
            return Err(invalid("Description payload exceeds its node limit"));
        }
        if depth > PAYLOAD_MAX_DEPTH {
            return Err(invalid("Description payload exceeds its depth limit"));
        }
        Ok(())
    }
    fn object(
        &mut self,
        properties: &[SchemaProperty],
        object: &'a serde_json::Map<String, Value>,
        discriminator: Option<&str>,
        depth: usize,
    ) -> ViewResult<()> {
        let extra = usize::from(discriminator.is_some());
        if object.len() > properties.len() + extra
            || object.keys().any(|key| {
                Some(key.as_str()) != discriminator
                    && !properties.iter().any(|property| property.name == key)
            })
        {
            return Err(invalid("Description payload has an undeclared property"));
        }
        for property in properties {
            match object.get(property.name) {
                Some(value) => self.node(property.node, value, depth + 1)?,
                None if property.required => {
                    return Err(invalid(
                        "Description payload is missing a required property",
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }
    fn node(&mut self, node: &SchemaNode, value: &'a Value, depth: usize) -> ViewResult<()> {
        self.enter(depth)?;
        match node {
            SchemaNode::Object(properties) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("Description property requires an object"))?;
                self.object(properties, object, None, depth)?;
            }
            SchemaNode::Array { item, max_items } => {
                let values = value
                    .as_array()
                    .ok_or_else(|| invalid("Description property requires an array"))?;
                if values.len() > *max_items {
                    return Err(invalid("Description array exceeds its limit"));
                }
                for value in values {
                    self.node(item, value, depth + 1)?;
                }
            }
            SchemaNode::Text { max_bytes } => {
                let text = value
                    .as_str()
                    .ok_or_else(|| invalid("Description property requires text"))?;
                if text.len() > *max_bytes {
                    return Err(invalid("Description text exceeds its limit"));
                }
            }
            SchemaNode::FieldPath => {
                let path = value
                    .as_str()
                    .ok_or_else(|| invalid("Description field reference requires text"))?;
                if !field_path(path) {
                    return Err(invalid("Description field reference has an invalid path"));
                }
                self.collector.push_borrowed(None, path)?;
            }
            SchemaNode::FieldHandle => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("Description field handle requires an object"))?;
                if object.len() != 2 || object.keys().any(|key| key != "model" && key != "name") {
                    return Err(invalid(
                        "Description field handle requires exactly model and name",
                    ));
                }
                let model = object
                    .get("model")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Description field handle requires a model"))?;
                let path = object
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Description field handle requires a field path"))?;
                if !model_reference(model) || !field_path(path) {
                    return Err(invalid(
                        "Description field handle has an invalid model or path",
                    ));
                }
                self.collector.push_borrowed(Some(model), path)?;
            }
            SchemaNode::ScopedDomain => {
                let result = (|| {
                    let object = value
                        .as_object()
                        .filter(|object| {
                            object.len() == 2
                                && object.contains_key("model")
                                && object.contains_key("ast")
                        })
                        .ok_or_else(|| {
                            invalid("Description scoped domain requires exactly model and ast")
                        })?;
                    let model = object
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|model| model_reference(model))
                        .ok_or_else(|| invalid("Description scoped domain has an invalid model"))?;
                    let ast = object.get("ast").ok_or_else(|| {
                        invalid("Description scoped domain requires exactly model and ast")
                    })?;
                    let slot = Slot {
                        panel_syntax: false,
                        kind: ExpressionKind::Domain,
                        scope: None,
                        parent_model: None,
                        scoped_model: Some(model),
                    };
                    let height = collect_value(ast, slot, self.budget, &mut self.collector)?;
                    if depth.saturating_add(1).saturating_add(height) > PAYLOAD_MAX_DEPTH {
                        return Err(invalid("Description payload exceeds its depth limit"));
                    }
                    Ok(())
                })();
                if result.is_err() {
                    self.budget.poison();
                }
                result?;
            }
            SchemaNode::Bool if !value.is_boolean() => {
                return Err(invalid("Description property requires a boolean"));
            }
            SchemaNode::Integer if value.as_i64().is_none() => {
                return Err(invalid("Description property requires a signed integer"));
            }
            SchemaNode::Bool | SchemaNode::Integer => {}
            SchemaNode::GroupBy { scope } => {
                let text = value
                    .as_str()
                    .ok_or_else(|| invalid("Description group_by requires text"))?;
                let path = crate::expression_references::parse_group_by(text)?;
                self.collector.push_borrowed(scope.model(), path)?;
            }
            SchemaNode::TaggedUnion {
                discriminator,
                variants,
            } => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("Description tagged union requires an object"))?;
                let tag = object
                    .get(*discriminator)
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid("Description tagged union requires a text discriminator")
                    })?;
                let variant = variants
                    .iter()
                    .find(|variant| variant.tag == tag)
                    .ok_or_else(|| invalid("Description tagged union has an undeclared variant"))?;
                let SchemaNode::Object(properties) = variant.object else {
                    return Err(ViewError::InvariantViolation(
                        "description variant is not an object",
                    ));
                };
                // The selected object is a node of its own, as in a constructed payload.
                self.enter(depth + 1)?;
                self.object(properties, object, Some(discriminator), depth + 1)?;
            }
            SchemaNode::Expression {
                kind,
                scope,
                parent_model,
            } => {
                let slot = Slot {
                    panel_syntax: false,
                    scoped_model: None,
                    kind: *kind,
                    scope: scope.model(),
                    parent_model: *parent_model,
                };
                let height = collect_value(value, slot, self.budget, &mut self.collector)?;
                if depth.saturating_add(height) > PAYLOAD_MAX_DEPTH {
                    return Err(invalid("Description payload exceeds its depth limit"));
                }
            }
        }
        Ok(())
    }
}
