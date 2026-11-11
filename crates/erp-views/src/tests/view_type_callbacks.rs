//! Callback composition of a finished `ViewTypeDecl` (WP3a). The checked input has no public
//! constructor (WP4 owns the real one); these in-crate tests use the test-only one.
use crate::{
    CheckedViewInput, DescriptionBudget, DescriptionBuilder, DescriptionValue,
    DescriptionValueBuilder, MetadataFieldSet, SchemaNode, SchemaProperty, ViewError, ViewResult,
    ViewSchema, ViewTypeDecl, ViewTypeDeclBuilder,
};

static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static ROOT: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("label", &TEXT)]);
static OTHER_TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static OTHER_ROOT: SchemaNode =
    SchemaNode::Object(&[SchemaProperty::required("label", &OTHER_TEXT)]);

fn schema() -> ViewSchema {
    ViewSchema::new(1, &ROOT).unwrap()
}
fn ok_validate(_: &CheckedViewInput<'_>, budget: &DescriptionBudget) -> ViewResult<()> {
    budget.check()
}
fn refusing_validate(_: &CheckedViewInput<'_>, _: &DescriptionBudget) -> ViewResult<()> {
    Err(ViewError::InvalidDescription(
        "validator says no".to_owned(),
    ))
}
fn ok_build(
    _: &CheckedViewInput<'_>,
    root: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    root.object(|object| object.set("label", |value| value.text("x")))
}
fn failing_build(
    _: &CheckedViewInput<'_>,
    _: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    Err(ViewError::InvalidDescription("builder says no".to_owned()))
}
/// Returns a value made under a different request budget, with the right schema slot.
fn other_budget_build(
    _: &CheckedViewInput<'_>,
    _: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let other = DescriptionBudget::new();
    let mut smuggled = None;
    DescriptionBuilder::new(&schema(), &other).build(|root| {
        smuggled = Some(root.object(|object| object.set("label", |value| value.text("x")))?);
        root.object(|object| object.set("label", |value| value.text("y")))
    })?;
    smuggled.ok_or(ViewError::InvariantViolation("test setup"))
}
/// Returns a value built for another schema than the declared one.
fn other_schema_build(
    _: &CheckedViewInput<'_>,
    _: DescriptionValueBuilder<'_>,
) -> ViewResult<DescriptionValue> {
    let budget = DescriptionBudget::new();
    let other = ViewSchema::new(1, &OTHER_ROOT).unwrap();
    let mut smuggled = None;
    DescriptionBuilder::new(&other, &budget).build(|root| {
        smuggled = Some(root.object(|object| object.set("label", |value| value.text("x")))?);
        root.object(|object| object.set("label", |value| value.text("y")))
    })?;
    smuggled.ok_or(ViewError::InvariantViolation("test setup"))
}

fn decl(validate: crate::ValidateView, build: crate::BuildView) -> ViewTypeDecl {
    let builder: ViewTypeDeclBuilder = ViewTypeDecl::builder("web", "list")
        .wire_major(1)
        .renderer("list")
        .schema(&schema())
        .validator(validate)
        .builder(build)
        .metadata_fields(
            MetadataFieldSet::builder()
                .arch_references()
                .finish()
                .unwrap(),
        )
        .methods(&[]);
    builder.finish().unwrap()
}

/// declared schema and the caller's budget, so the payload it returns is schema-checked)
#[test]
fn declared_builder_output_is_wrapped_by_the_bounded_builder() {
    let input = CheckedViewInput::for_tests();
    let budget = DescriptionBudget::new();
    let decl = decl(ok_validate, ok_build);
    decl.validate(&input, &budget).unwrap();
    let payload = decl.build(&input, &budget).unwrap();
    assert_eq!(serde_json::to_string(&payload).unwrap(), r#"{"label":"x"}"#);
}

#[test]
fn declared_validator_and_builder_errors_propagate_and_stay_sticky() {
    let input = CheckedViewInput::for_tests();
    let budget = DescriptionBudget::new();
    let refusing = decl(refusing_validate, failing_build);
    assert_eq!(
        refusing.validate(&input, &budget).unwrap_err(),
        ViewError::InvalidDescription("validator says no".to_owned())
    );
    assert_eq!(
        refusing.build(&input, &budget).unwrap_err(),
        ViewError::InvalidDescription("builder says no".to_owned())
    );
    // A caught builder failure cannot be followed by a successful publication on this budget.
    assert_eq!(
        budget.check().unwrap_err(),
        ViewError::InvalidDescription(
            "Description remains refused after an earlier construction failure".to_owned()
        )
    );
}

/// / plan WP3a: a builder callback cannot return a value from another budget (or for
/// another schema slot) and have it published.
#[test]
fn builder_cannot_return_a_value_from_another_budget_or_schema() {
    let input = CheckedViewInput::for_tests();
    let budget = DescriptionBudget::new();
    assert_eq!(
        decl(ok_validate, other_budget_build)
            .build(&input, &budget)
            .unwrap_err(),
        ViewError::InvalidDescription(
            "Description value budget identity does not match its request".to_owned()
        )
    );
    let budget = DescriptionBudget::new();
    assert_eq!(
        decl(ok_validate, other_schema_build)
            .build(&input, &budget)
            .unwrap_err(),
        ViewError::InvalidDescription(
            "Description value schema identity does not match its declared slot".to_owned()
        )
    );
    assert!(budget.check().is_err());
}
