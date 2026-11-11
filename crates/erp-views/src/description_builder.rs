//! Schema-directed construction: no raw value constructor or per-child budget reset.
use super::description_budget::storage;
use crate::description_schema::{PAYLOAD_MAX_DEPTH, PAYLOAD_MAX_NODES};
use crate::expression_decode::{Slot, collect_document, encoded_len};
use crate::expression_references::{
    BudgetMeter, parse_group_by, walk_expression, walk_scoped_domain,
};
use crate::reference_index::Collector;
use crate::{
    DescriptionBudget, DescriptionReservation, ReferenceVisitor, SchemaNode, SchemaProperty,
    SchemaReferences, ViewError, ViewResult, ViewSchema, expression_preflight,
};
use erp_expr::wire::{ValidatedAst, WireDocument};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};

#[path = "builtin_wire.rs"]
mod builtin_wire;
pub use builtin_wire::AdmittedBuiltinView;

fn refuse(budget: &DescriptionBudget, message: &str) -> ViewError {
    budget.poison();
    ViewError::InvalidDescription(message.to_owned())
}
fn propagate<T>(budget: &DescriptionBudget, result: ViewResult<T>) -> ViewResult<T> {
    if result.is_err() {
        budget.poison();
    }
    result
}

/// One pure callback's structural builder over a validated schema and the request's shared budget.
pub struct DescriptionBuilder<'a> {
    schema: ViewSchema,
    budget: &'a DescriptionBudget,
}
impl<'a> DescriptionBuilder<'a> {
    /// Bind construction to the actual schema and shared request allowance.
    pub fn new(schema: &ViewSchema, budget: &'a DescriptionBudget) -> Self {
        Self {
            schema: schema.clone(),
            budget,
        }
    }
    /// Construct a closed payload. This grants no caller/model authority.
    pub fn build(
        &self,
        build: impl FnOnce(DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue>,
    ) -> ViewResult<DescriptionPayload> {
        self.budget.check()?;
        let value = propagate(
            self.budget,
            build(DescriptionValueBuilder {
                schema: self.schema.root_node(),
                budget: self.budget,
            }),
        )?;
        walk(
            self.schema.root_node(),
            &value,
            1,
            &mut Constructed {
                budget: self.budget,
                visited: 0,
                collector: None,
            },
        )?;
        self.budget.check()?;
        Ok(DescriptionPayload {
            schema: self.schema.clone(),
            value,
        })
    }
}

/// A private-field cursor: values can only be made for its schema and budget.
#[derive(Clone, Copy)]
pub struct DescriptionValueBuilder<'a> {
    schema: &'static SchemaNode,
    budget: &'a DescriptionBudget,
}
impl<'a> DescriptionValueBuilder<'a> {
    /// The shared construction allowance of the request that owns this cursor.
    ///
    /// Callbacks reserve their intermediate storage through this allowance before allocating.
    /// Reservations and failures are shared with the whole request; the returned reference
    /// neither creates a new allowance nor grants model or field authority.
    ///
    /// ```
    /// use erp_views::{DescriptionBudget, DescriptionBuilder, SchemaNode, ViewSchema};
    /// static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 32 };
    /// let schema = ViewSchema::new(1, &TEXT)?;
    /// let budget = DescriptionBudget::new();
    /// let _payload = DescriptionBuilder::new(&schema, &budget).build(|root| {
    ///     let _scratch_storage = root.budget().reserve(8)?;
    ///     root.text("name")
    /// })?;
    /// # Ok::<(), erp_views::ViewError>(())
    /// ```
    #[must_use]
    pub fn budget(&self) -> &'a DescriptionBudget {
        self.budget
    }
    fn value(&self, kind: Kind) -> DescriptionValue {
        DescriptionValue {
            schema: self.schema,
            budget: self.budget.clone(),
            kind,
        }
    }
    fn copy_text(&self, text: &str) -> ViewResult<(String, DescriptionReservation)> {
        self.budget.check()?;
        let mut reservation = self.budget.reserve(text.len())?;
        let mut value = String::new();
        if value.try_reserve_exact(text.len()).is_err() {
            return Err(refuse(self.budget, "Description text allocation failed"));
        }
        if value.capacity() > text.len() {
            reservation.grow(value.capacity() - text.len())?;
        }
        value.push_str(text);
        Ok((value, reservation))
    }
    fn string(&self, text: &str) -> ViewResult<DescriptionValue> {
        let (value, reservation) = self.copy_text(text)?;
        Ok(self.value(Kind::Text {
            value,
            _reservation: reservation,
        }))
    }
    /// Copy ordinary display text, never interpreting it as a field reference.
    pub fn text(&self, text: &str) -> ViewResult<DescriptionValue> {
        match self.schema {
            SchemaNode::Text { max_bytes } if text.len() <= *max_bytes => self.string(text),
            SchemaNode::Text { .. } => Err(refuse(
                self.budget,
                "Description text exceeds its schema limit",
            )),
            _ => Err(refuse(
                self.budget,
                "Description value does not match its text schema slot",
            )),
        }
    }
    /// Copy a validated static model-field path; host authorization is still required.
    pub fn field_path(&self, path: &str) -> ViewResult<DescriptionValue> {
        if !matches!(self.schema, SchemaNode::FieldPath)
            || !super::description_schema::field_path(path)
        {
            return Err(refuse(
                self.budget,
                "Description field reference has an invalid path or schema slot",
            ));
        }
        self.string(path)
    }
    /// Copy an explicit model/name handle without dropping its model scope.
    pub fn field_handle(&self, model: &str, path: &str) -> ViewResult<DescriptionValue> {
        if !matches!(self.schema, SchemaNode::FieldHandle)
            || !super::description_schema::model_reference(model)
            || !super::description_schema::field_path(path)
        {
            return Err(refuse(
                self.budget,
                "Description field handle has an invalid model, path or schema slot",
            ));
        }
        let (model, model_reservation) = self.copy_text(model)?;
        let (path, path_reservation) = self.copy_text(path)?;
        Ok(self.value(Kind::FieldHandle {
            model,
            path,
            _model_reservation: model_reservation,
            _path_reservation: path_reservation,
        }))
    }
    /// Construct a schema-declared boolean.
    pub fn boolean(&self, value: bool) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        if !matches!(self.schema, SchemaNode::Bool) {
            return Err(refuse(
                self.budget,
                "Description value does not match its boolean schema slot",
            ));
        }
        Ok(self.value(Kind::Bool(value)))
    }
    /// Construct a schema-declared signed integer.
    pub fn integer(&self, value: i64) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        if !matches!(self.schema, SchemaNode::Integer) {
            return Err(refuse(
                self.budget,
                "Description value does not match its integer schema slot",
            ));
        }
        Ok(self.value(Kind::Integer(value)))
    }
    /// Embed an admitted expression as its wire document in an expression slot.
    ///
    /// The reservation is derived from the admitted tree and charged before the document is
    /// built, so a refusal happens with nothing allocated; afterwards the unused part is released
    /// down to the capacities the document actually keeps.
    pub fn expression(&self, admitted: &ValidatedAst) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        let SchemaNode::Expression {
            kind, parent_model, ..
        } = self.schema
        else {
            return Err(refuse(
                self.budget,
                "Description value does not match its expression schema slot",
            ));
        };
        let preflight = propagate(self.budget, expression_preflight(admitted.as_expr()))?;
        // The same walker the payload visitors use admits the references first: an unsupported
        // construct, an undeclared parent or exhausted shared work refuses before any allocation.
        let height = propagate(
            self.budget,
            walk_expression(
                admitted.as_expr(),
                *kind,
                parent_model.is_some(),
                &mut BudgetMeter(self.budget),
                |_| Ok(()),
            ),
        )?;
        let mut reservation = self.budget.reserve(preflight.reserve_bytes())?;
        // Admission retained the exact canonical length from erp-expr's own wire serializer.
        // Charge the shared expression allowance before constructing a new wire document.
        self.budget
            .charge_expression_bytes(admitted.encoded_len())?;
        let mut document = WireDocument::from_expr(admitted.as_expr()).map_err(|error| {
            refuse(
                self.budget,
                &format!("Description expression could not be encoded: {error}"),
            )
        })?;
        document.nodes.shrink_to_fit();
        let retained = propagate(
            self.budget,
            crate::expression_preflight::retained_bytes(&document),
        )?;
        reservation.shrink_to(retained)?;
        let encoded = propagate(self.budget, encoded_len(&document))?;
        if encoded != admitted.encoded_len() {
            self.budget.poison();
            return Err(ViewError::InvariantViolation(
                "admitted expression encoded length changed during construction",
            ));
        }
        self.budget.check()?;
        Ok(self.value(Kind::Expression {
            document,
            retained,
            height,
            _reservation: reservation,
        }))
    }
    /// Construct a closed scoped-domain document from an admitted AST and descriptive model.
    ///
    /// Criterion LHS paths resolve on `model`; all RHS names resolve from the checked root.
    /// Both roles survive when their spellings coincide. The model and wire-document storage
    /// are reserved together before copying. The host must compare the model with the real
    /// relation and authorize every independently derived reference.
    ///
    /// This generic slot does not apply the panel's category allowlist or RHS grammar; call
    /// [`crate::validate_panel_domain`] separately for version-one panel syntax.
    ///
    /// # Errors
    /// Wrong schema slot, invalid model/domain, unsupported quantified operator, or exhausted
    /// shared storage/work/expression bytes. Every refusal poisons the request allowance.
    ///
    /// ```
    /// use erp_views::{DescriptionBudget, DescriptionBuilder, SchemaNode, ViewSchema};
    /// static DOMAIN: SchemaNode = SchemaNode::ScopedDomain;
    /// let admitted = erp_expr::wire::ValidatedAst::from_expr(&erp_expr::parse("[]")?)?;
    /// let schema = ViewSchema::new(1, &DOMAIN)?;
    /// let budget = DescriptionBudget::new();
    /// let payload = DescriptionBuilder::new(&schema, &budget)
    ///     .build(|root| root.scoped_domain("example.tag", &admitted))?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn scoped_domain(
        &self,
        model: &str,
        admitted: &ValidatedAst,
    ) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        if !matches!(self.schema, SchemaNode::ScopedDomain) {
            return Err(refuse(
                self.budget,
                "Description value does not match its scoped domain schema slot",
            ));
        }
        if !super::description_schema::model_reference(model) {
            return Err(refuse(
                self.budget,
                "Description scoped domain has an invalid model",
            ));
        }
        let preflight = propagate(self.budget, expression_preflight(admitted.as_expr()))?;
        let height = propagate(
            self.budget,
            walk_scoped_domain(
                admitted.as_expr(),
                &mut BudgetMeter(self.budget),
                |_| Ok(()),
            ),
        )?;
        let reserve = preflight
            .reserve_bytes()
            .checked_add(model.len())
            .ok_or_else(|| {
                refuse(
                    self.budget,
                    "Description scoped domain reservation overflow",
                )
            })?;
        let mut reservation = self.budget.reserve(reserve)?;
        self.budget
            .charge_expression_bytes(admitted.encoded_len())?;
        let mut copied_model = String::new();
        if copied_model.try_reserve_exact(model.len()).is_err() {
            return Err(refuse(
                self.budget,
                "Description scoped domain allocation failed",
            ));
        }
        if copied_model.capacity() > model.len() {
            reservation.grow(copied_model.capacity() - model.len())?;
        }
        copied_model.push_str(model);
        let mut document = WireDocument::from_expr(admitted.as_expr()).map_err(|_| {
            refuse(
                self.budget,
                "Description scoped domain could not be encoded",
            )
        })?;
        document.nodes.shrink_to_fit();
        let retained = propagate(
            self.budget,
            crate::expression_preflight::retained_bytes(&document),
        )?
        .checked_add(copied_model.capacity())
        .ok_or_else(|| {
            refuse(
                self.budget,
                "Description scoped domain reservation overflow",
            )
        })?;
        reservation.shrink_to(retained)?;
        let encoded = propagate(self.budget, encoded_len(&document))?;
        if encoded != admitted.encoded_len() {
            self.budget.poison();
            return Err(ViewError::InvariantViolation(
                "admitted expression encoded length changed during construction",
            ));
        }
        Ok(self.value(Kind::ScopedDomain {
            model: copied_model,
            document,
            retained,
            height,
            _reservation: reservation,
        }))
    }
    /// Copy a group-by entry: a static field path with at most one supported temporal suffix.
    pub fn group_by(&self, text: &str) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        if !matches!(self.schema, SchemaNode::GroupBy { .. }) {
            return Err(refuse(
                self.budget,
                "Description value does not match its group_by schema slot",
            ));
        }
        propagate(self.budget, parse_group_by(text))?;
        self.string(text)
    }
    /// Build the branch a tagged-union slot declares for `tag`. The branch builder receives the
    /// variant's own closed object schema, so it cannot build another branch's members or set the
    /// discriminator. The union's tag is written first when the payload is serialized.
    pub fn variant(
        &self,
        tag: &str,
        build: impl FnOnce(DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue>,
    ) -> ViewResult<DescriptionValue> {
        self.budget.check()?;
        let SchemaNode::TaggedUnion {
            discriminator,
            variants,
        } = self.schema
        else {
            return Err(refuse(
                self.budget,
                "Description value does not match its tagged union schema slot",
            ));
        };
        let variant = variants
            .iter()
            .find(|variant| variant.tag == tag)
            .ok_or_else(|| {
                refuse(
                    self.budget,
                    "Description tagged union has an undeclared variant",
                )
            })?;
        // The boxed branch is admitted before it is built.
        let reservation = self
            .budget
            .reserve(std::mem::size_of::<DescriptionValue>())?;
        let branch = propagate(
            self.budget,
            build(DescriptionValueBuilder {
                schema: variant.object,
                budget: self.budget,
            }),
        )?;
        identity(variant.object, &branch, self.budget)?;
        self.budget.check()?;
        Ok(self.value(Kind::Variant {
            discriminator,
            tag: variant.tag,
            branch: Box::new(branch),
            _reservation: reservation,
        }))
    }
    /// Allocate a bounded item array before invoking its item constructors.
    pub fn array(
        &self,
        count: usize,
        mut build: impl FnMut(usize, DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue>,
    ) -> ViewResult<DescriptionValue> {
        let SchemaNode::Array { item, max_items } = self.schema else {
            return Err(refuse(
                self.budget,
                "Description value does not match its array schema slot",
            ));
        };
        if count > *max_items {
            return Err(refuse(
                self.budget,
                "Description array exceeds its schema limit",
            ));
        }
        let (mut values, reservation) = storage::<DescriptionValue>(self.budget, count)?;
        for index in 0..count {
            let value = propagate(
                self.budget,
                build(
                    index,
                    DescriptionValueBuilder {
                        schema: item,
                        budget: self.budget,
                    },
                ),
            )?;
            identity(item, &value, self.budget)?;
            values.push(value);
        }
        self.budget.check()?;
        Ok(self.value(Kind::Array {
            values,
            _reservation: reservation,
        }))
    }
    /// Allocate closed object storage, then fill only declared properties.
    pub fn object(
        &self,
        build: impl FnOnce(&mut DescriptionObjectBuilder<'_>) -> ViewResult<()>,
    ) -> ViewResult<DescriptionValue> {
        let SchemaNode::Object(properties) = self.schema else {
            return Err(refuse(
                self.budget,
                "Description value does not match its object schema slot",
            ));
        };
        let (values, reservation) =
            storage::<(&'static str, DescriptionValue)>(self.budget, properties.len())?;
        let mut object = DescriptionObjectBuilder {
            properties,
            budget: self.budget,
            values,
        };
        propagate(self.budget, build(&mut object))?;
        if properties.iter().any(|property| {
            property.required && !object.values.iter().any(|(name, _)| *name == property.name)
        }) {
            return Err(refuse(
                self.budget,
                "Description payload is missing a required property",
            ));
        }
        self.budget.check()?;
        Ok(self.value(Kind::Object {
            values: object.values,
            _reservation: reservation,
        }))
    }
}

/// A closed object cursor. There is no unchecked insertion or mutable raw-value accessor.
pub struct DescriptionObjectBuilder<'a> {
    properties: &'static [SchemaProperty],
    budget: &'a DescriptionBudget,
    values: Vec<(&'static str, DescriptionValue)>,
}
impl DescriptionObjectBuilder<'_> {
    /// Fill one declared property exactly once, under its own child schema.
    pub fn set(
        &mut self,
        name: &str,
        build: impl FnOnce(DescriptionValueBuilder<'_>) -> ViewResult<DescriptionValue>,
    ) -> ViewResult<()> {
        self.budget.check()?;
        let property = self
            .properties
            .iter()
            .find(|property| property.name == name)
            .ok_or_else(|| {
                refuse(
                    self.budget,
                    "Description payload has an undeclared property",
                )
            })?;
        if self.values.iter().any(|(name, _)| *name == property.name) {
            return Err(refuse(
                self.budget,
                "Description payload has a duplicate property",
            ));
        }
        let value = propagate(
            self.budget,
            build(DescriptionValueBuilder {
                schema: property.node,
                budget: self.budget,
            }),
        )?;
        identity(property.node, &value, self.budget)?;
        self.values.push((property.name, value));
        Ok(())
    }
}
/// A non-cloneable, schema/budget-bound constructed value with no public raw constructor.
pub struct DescriptionValue {
    schema: &'static SchemaNode,
    budget: DescriptionBudget,
    kind: Kind,
}
enum Kind {
    ScopedDomain {
        model: String,
        document: WireDocument,
        retained: usize,
        height: usize,
        _reservation: DescriptionReservation,
    },
    FieldHandle {
        model: String,
        path: String,
        _model_reservation: DescriptionReservation,
        _path_reservation: DescriptionReservation,
    },
    Text {
        value: String,
        _reservation: DescriptionReservation,
    },
    Bool(bool),
    Integer(i64),
    Expression {
        document: WireDocument,
        retained: usize,
        /// Logical height of the tree the document was built from.
        height: usize,
        _reservation: DescriptionReservation,
    },
    Variant {
        discriminator: &'static str,
        tag: &'static str,
        branch: Box<DescriptionValue>,
        _reservation: DescriptionReservation,
    },
    Array {
        values: Vec<DescriptionValue>,
        _reservation: DescriptionReservation,
    },
    Object {
        values: Vec<(&'static str, DescriptionValue)>,
        _reservation: DescriptionReservation,
    },
}
impl DescriptionValue {
    /// Heap bytes this value keeps alive, from the capacities it actually holds. After a
    /// successful build this equals what the request budget still counts for it.
    pub fn retained_bytes(&self) -> usize {
        use std::mem::size_of;
        match &self.kind {
            Kind::Text { value, .. } => value.capacity(),
            Kind::FieldHandle { model, path, .. } => {
                model.capacity().saturating_add(path.capacity())
            }
            Kind::Bool(_) | Kind::Integer(_) => 0,
            Kind::Expression { retained, .. } | Kind::ScopedDomain { retained, .. } => *retained,
            Kind::Variant { branch, .. } => {
                size_of::<DescriptionValue>().saturating_add(branch.retained_bytes())
            }
            Kind::Array { values, .. } => values.iter().fold(
                values
                    .capacity()
                    .saturating_mul(size_of::<DescriptionValue>()),
                |total, value| total.saturating_add(value.retained_bytes()),
            ),
            Kind::Object { values, .. } => values.iter().fold(
                values
                    .capacity()
                    .saturating_mul(size_of::<(&'static str, DescriptionValue)>()),
                |total, (_, value)| total.saturating_add(value.retained_bytes()),
            ),
        }
    }
}
impl std::fmt::Debug for DescriptionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DescriptionValue").finish_non_exhaustive()
    }
}

/// Closed constructed structure. It is not an ACL witness and cannot be served without host checks.
#[derive(Debug)]
pub struct DescriptionPayload {
    schema: ViewSchema,
    value: DescriptionValue,
}
impl DescriptionPayload {
    /// Heap bytes the payload's values keep alive (see [`DescriptionValue::retained_bytes`]).
    pub fn retained_bytes(&self) -> usize {
        self.value.retained_bytes()
    }
}
impl Serialize for DescriptionPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value
            .budget
            .check()
            .map_err(serde::ser::Error::custom)?;
        let result = self.value.serialize(serializer)?;
        self.value
            .budget
            .check()
            .map_err(serde::ser::Error::custom)?;
        Ok(result)
    }
}
impl Serialize for DescriptionValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.budget.check().map_err(serde::ser::Error::custom)?;
        match &self.kind {
            Kind::Text { value, .. } => serializer.serialize_str(value),
            Kind::FieldHandle { model, path, .. } => {
                let mut object = serializer.serialize_map(Some(2))?;
                object.serialize_entry("model", model)?;
                object.serialize_entry("name", path)?;
                object.end()
            }
            Kind::ScopedDomain {
                model, document, ..
            } => {
                let mut object = serializer.serialize_map(Some(2))?;
                object.serialize_entry("model", model)?;
                object.serialize_entry("ast", document)?;
                object.end()
            }
            Kind::Expression { document, .. } => document.serialize(serializer),
            Kind::Variant {
                discriminator,
                tag,
                branch,
                ..
            } => {
                let Kind::Object { values, .. } = &branch.kind else {
                    return Err(serde::ser::Error::custom(
                        "description variant branch is not an object",
                    ));
                };
                let mut object = serializer.serialize_map(Some(values.len() + 1))?;
                object.serialize_entry(discriminator, tag)?;
                for (name, value) in values {
                    object.serialize_entry(name, value)?;
                }
                object.end()
            }
            Kind::Bool(value) => serializer.serialize_bool(*value),
            Kind::Integer(value) => serializer.serialize_i64(*value),
            Kind::Array { values, .. } => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Kind::Object { values, .. } => {
                let mut object = serializer.serialize_map(Some(values.len()))?;
                for (name, value) in values {
                    object.serialize_entry(name, value)?;
                }
                object.end()
            }
        }
    }
}
fn identity(
    schema: &SchemaNode,
    value: &DescriptionValue,
    budget: &DescriptionBudget,
) -> ViewResult<()> {
    if !std::ptr::eq(schema, value.schema) {
        return Err(refuse(
            budget,
            "Description value schema identity does not match its declared slot",
        ));
    }
    if !budget.same_budget(&value.budget) {
        return Err(refuse(
            budget,
            "Description value budget identity does not match its request",
        ));
    }
    budget.check()
}
/// State of one walk over a constructed payload. With a collector it derives references, decoding
/// every expression again; without one it only validates structure (what `build` does).
struct Constructed<'a, 'b> {
    budget: &'b DescriptionBudget,
    visited: usize,
    collector: Option<Collector<'a>>,
}
fn walk<'a>(
    schema: &SchemaNode,
    value: &'a DescriptionValue,
    depth: usize,
    ctx: &mut Constructed<'a, '_>,
) -> ViewResult<()> {
    let budget = ctx.budget;
    identity(schema, value, budget)?;
    ctx.visited += 1;
    if ctx.visited > PAYLOAD_MAX_NODES {
        return Err(refuse(budget, "Description payload exceeds its node limit"));
    }
    if depth > PAYLOAD_MAX_DEPTH {
        return Err(refuse(
            budget,
            "Description payload exceeds its depth limit",
        ));
    }
    match (schema, &value.kind) {
        (SchemaNode::Text { max_bytes }, Kind::Text { value, .. }) if value.len() <= *max_bytes => {
        }
        (SchemaNode::FieldPath, Kind::Text { value, .. })
            if super::description_schema::field_path(value) =>
        {
            if let Some(collector) = &mut ctx.collector {
                propagate(budget, collector.push_borrowed(None, value))?;
            }
        }
        (SchemaNode::FieldHandle, Kind::FieldHandle { model, path, .. })
            if super::description_schema::model_reference(model)
                && super::description_schema::field_path(path) =>
        {
            if let Some(collector) = &mut ctx.collector {
                propagate(budget, collector.push_borrowed(Some(model), path))?;
            }
        }
        (
            SchemaNode::ScopedDomain,
            Kind::ScopedDomain {
                model,
                document,
                height,
                ..
            },
        ) => {
            if !super::description_schema::model_reference(model) {
                return Err(refuse(
                    budget,
                    "Description scoped domain has an invalid model",
                ));
            }
            let height = match &mut ctx.collector {
                Some(collector) => {
                    let slot = Slot {
                        panel_syntax: false,
                        kind: crate::ExpressionKind::Domain,
                        scope: None,
                        parent_model: None,
                        scoped_model: Some(model),
                    };
                    propagate(budget, collect_document(document, slot, budget, collector))?
                }
                None => *height,
            };
            if depth.saturating_add(1).saturating_add(height) > PAYLOAD_MAX_DEPTH {
                return Err(refuse(
                    budget,
                    "Description payload exceeds its depth limit",
                ));
            }
        }
        (SchemaNode::Bool, Kind::Bool(_)) | (SchemaNode::Integer, Kind::Integer(_)) => {}
        (SchemaNode::GroupBy { scope }, Kind::Text { value, .. }) => {
            let path = propagate(budget, parse_group_by(value))?;
            if let Some(collector) = &mut ctx.collector {
                propagate(budget, collector.push_borrowed(scope.model(), path))?;
            }
        }
        (
            SchemaNode::Expression {
                kind,
                scope,
                parent_model,
            },
            Kind::Expression {
                document, height, ..
            },
        ) => {
            // A reference walk decodes the retained document again and derives the references
            // from that tree; the structural walk trusts only the height recorded at construction.
            let height = match &mut ctx.collector {
                Some(collector) => {
                    let slot = Slot {
                        panel_syntax: false,
                        scoped_model: None,
                        kind: *kind,
                        scope: scope.model(),
                        parent_model: *parent_model,
                    };
                    propagate(budget, collect_document(document, slot, budget, collector))?
                }
                None => *height,
            };
            if depth.saturating_add(height) > PAYLOAD_MAX_DEPTH {
                return Err(refuse(
                    budget,
                    "Description payload exceeds its depth limit",
                ));
            }
        }
        (SchemaNode::Array { item, max_items }, Kind::Array { values, .. })
            if values.len() <= *max_items =>
        {
            for value in values {
                walk(item, value, depth + 1, ctx)?;
            }
        }
        (SchemaNode::TaggedUnion { variants, .. }, Kind::Variant { tag, branch, .. }) => {
            let variant = variants
                .iter()
                .find(|variant| variant.tag == *tag)
                .ok_or_else(|| {
                    refuse(budget, "Description tagged union has an undeclared variant")
                })?;
            walk(variant.object, branch, depth + 1, ctx)?;
        }
        (SchemaNode::Object(properties), Kind::Object { values, .. }) => {
            if values.len() > properties.len() {
                return Err(refuse(
                    budget,
                    "Description payload has undeclared properties",
                ));
            }
            for property in *properties {
                let mut matches = values.iter().filter(|(name, _)| *name == property.name);
                match matches.next() {
                    Some((_, value)) => walk(property.node, value, depth + 1, ctx)?,
                    None if property.required => {
                        return Err(refuse(
                            budget,
                            "Description payload is missing a required property",
                        ));
                    }
                    None => {}
                }
                if matches.next().is_some() {
                    return Err(refuse(
                        budget,
                        "Description payload has a duplicate property",
                    ));
                }
            }
            if values
                .iter()
                .any(|(name, _)| !properties.iter().any(|property| property.name == *name))
            {
                return Err(refuse(
                    budget,
                    "Description payload has an undeclared property",
                ));
            }
        }
        _ => {
            return Err(refuse(
                budget,
                "Description payload does not match its schema",
            ));
        }
    }
    Ok(())
}
impl ReferenceVisitor {
    /// Independently derive references of one node in a declared `nodes` arena.
    ///
    /// The item schema comes from this registered visitor, never from the builder. The host
    /// first walks the complete payload with [`Self::payload_references`], then uses these
    /// node-local references to remove an unreadable search item as a whole. This grants no
    /// authorization and does not validate arena edges; the builtin wire adapter does that.
    ///
    /// # Errors
    /// Schema/budget mismatch, missing declared arena, out-of-bounds index, invalid node or
    /// exhausted shared admission. Every refusal poisons the request budget.
    pub fn payload_node_references<'a>(
        &self,
        payload: &'a DescriptionPayload,
        index: usize,
        budget: &DescriptionBudget,
    ) -> ViewResult<SchemaReferences<'a>> {
        let result = (|| {
            budget.check()?;
            if !payload.value.budget.same_budget(budget)
                || self.schema().revision() != payload.schema.revision()
                || !std::ptr::eq(self.schema().root_node(), payload.schema.root_node())
            {
                return Err(refuse(
                    budget,
                    "Description node visitor identity does not match its payload",
                ));
            }
            let SchemaNode::Object(properties) = self.schema().root_node() else {
                return Err(refuse(
                    budget,
                    "Description schema has no declared node arena",
                ));
            };
            let item = properties
                .iter()
                .find_map(|property| {
                    if property.name != "nodes" {
                        return None;
                    }
                    match property.node {
                        SchemaNode::Array { item, .. } => Some(*item),
                        _ => None,
                    }
                })
                .ok_or_else(|| refuse(budget, "Description schema has no declared node arena"))?;
            let Kind::Object { values, .. } = &payload.value.kind else {
                return Err(refuse(budget, "Description payload has no node arena"));
            };
            let nodes = values
                .iter()
                .find_map(|(key, value)| {
                    if *key != "nodes" {
                        return None;
                    }
                    match &value.kind {
                        Kind::Array { values, .. } => Some(values),
                        _ => None,
                    }
                })
                .ok_or_else(|| refuse(budget, "Description payload has no node arena"))?;
            let node = nodes.get(index).ok_or_else(|| {
                refuse(
                    budget,
                    "Description node index is outside its declared arena",
                )
            })?;
            let mut ctx = Constructed {
                budget,
                visited: 0,
                collector: Some(Collector::new(budget)?),
            };
            walk(item, node, 1, &mut ctx)?;
            ctx.collector
                .take()
                .ok_or(ViewError::InvariantViolation(
                    "description reference collector missing",
                ))?
                .finish()
        })();
        propagate(budget, result)
    }
    /// Independently walk all slots in a constructed payload under its actual request budget.
    /// Expression slots are decoded again from their retained wire documents, never read from
    /// builder metadata. This still returns untrusted model references for the host's per-path
    /// authorization.
    pub fn payload_references<'a>(
        &self,
        payload: &'a DescriptionPayload,
        budget: &DescriptionBudget,
    ) -> ViewResult<SchemaReferences<'a>> {
        if self.schema().revision() != payload.schema.revision()
            || !std::ptr::eq(self.schema().root_node(), payload.schema.root_node())
        {
            return Err(refuse(
                budget,
                "Description payload schema identity does not match its registered visitor",
            ));
        }
        let mut ctx = Constructed {
            budget,
            visited: 0,
            collector: Some(propagate(budget, Collector::new(budget))?),
        };
        walk(self.schema().root_node(), &payload.value, 1, &mut ctx)?;
        let collector = ctx.collector.take().ok_or(ViewError::InvariantViolation(
            "description reference collector missing",
        ))?;
        collector.finish()
    }
}
