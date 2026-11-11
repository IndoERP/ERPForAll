//! Private structural builders cannot omit schema checks or reset request budgets.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_views::{
    DescriptionBudget, DescriptionBuilder, DescriptionEncoder, FieldReference, LimitKind,
    ReferenceScope, ReferenceVisitor, SchemaNode, SchemaProperty, SchemaVariant, ViewError,
    ViewLimits, ViewSchema,
};
use serde_json::json;

static TEXT: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static FIELD: SchemaNode = SchemaNode::FieldPath;
static FIELDS: SchemaNode = SchemaNode::Array {
    item: &FIELD,
    max_items: 16,
};
static ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("label", &TEXT),
    SchemaProperty::required("fields", &FIELDS),
]);

#[test]
fn bounded_tree_round_trip_keeps_every_schema_declared_reference() {
    let schema = ViewSchema::new(1, &ROOT).unwrap();
    let budget = DescriptionBudget::with_limit(4096).unwrap();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            root.object(|object| {
                object.set("label", |value| value.text("Example"))?;
                object.set("fields", |value| {
                    value.array(2, |index, value| {
                        value.field_path(if index == 0 {
                            "name"
                        } else {
                            "company_id.partner_id.email"
                        })
                    })
                })
            })
        })
        .unwrap();
    let visitor = ReferenceVisitor::new(schema);
    let refs = visitor.payload_references(&payload, &budget).unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![
            erp_views::FieldReference::RootPath("company_id.partner_id.email"),
            erp_views::FieldReference::RootPath("name")
        ]
    );
    let bytes = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT)
        .encode(&payload)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(bytes.as_json_bytes()).unwrap(),
        json!({"label":"Example", "fields":["name", "company_id.partner_id.email"]})
    );
    drop(bytes);
    drop(refs);
    drop(payload);
    budget.reserve(4096).unwrap();
}

#[test]
fn missing_unknown_and_caught_invalid_properties_never_produce_a_payload() {
    let schema = ViewSchema::new(1, &ROOT).unwrap();
    let budget = DescriptionBudget::new();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.object(|_| Ok(())))
        .unwrap_err();
    assert!(error.to_string().contains("required"));
    let budget = DescriptionBudget::new();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            root.object(|object| {
                object.set("label", |value| value.text("Example"))?;
                object.set("fields", |value| {
                    value.array(0, |_, value| value.field_path("name"))
                })?;
                let error = object
                    .set("unknown", |value| value.text("secret"))
                    .unwrap_err();
                assert!(error.to_string().contains("undeclared"));
                Ok(())
            })
        })
        .unwrap_err();
    assert!(error.to_string().contains("earlier construction failure"));
}

#[test]
fn values_cannot_reset_the_shared_budget_or_use_another_schema() {
    let schema = ViewSchema::new(1, &TEXT).unwrap();
    let original = DescriptionBudget::with_limit(10).unwrap();
    let other = DescriptionBudget::new();
    let mut foreign = None;
    DescriptionBuilder::new(&schema, &other)
        .build(|root| {
            foreign = Some(root.text("foreign")?);
            root.text("other")
        })
        .unwrap();
    let error = DescriptionBuilder::new(&schema, &original)
        .build(|_| Ok(foreign.take().unwrap()))
        .unwrap_err();
    assert!(error.to_string().contains("budget identity"));
    static OTHER_TEXT: SchemaNode = SchemaNode::Text { max_bytes: 63 };
    let other_schema = ViewSchema::new(1, &OTHER_TEXT).unwrap();
    let same_budget = DescriptionBudget::new();
    let mut wrong_schema = None;
    DescriptionBuilder::new(&other_schema, &same_budget)
        .build(|root| {
            wrong_schema = Some(root.text("different")?);
            root.text("holder")
        })
        .unwrap();
    let error = DescriptionBuilder::new(&schema, &same_budget)
        .build(|_| Ok(wrong_schema.take().unwrap()))
        .unwrap_err();
    assert!(error.to_string().contains("schema identity"));
    let original = DescriptionBudget::with_limit(10).unwrap();
    let _retained = original.reserve(8).unwrap();
    let error = DescriptionBuilder::new(&schema, &original)
        .build(|root| root.text("12345"))
        .unwrap_err();
    assert!(error.to_string().contains("description bytes"));
}

#[test]
fn sticky_structural_failure_does_not_masquerade_as_byte_exhaustion() {
    let schema = ViewSchema::new(1, &TEXT).unwrap();
    let budget = DescriptionBudget::new();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|value| value.boolean(true))
        .unwrap_err();
    assert!(matches!(error, erp_views::ViewError::InvalidDescription(_)));
    let error = budget.check().unwrap_err();
    assert!(
        matches!(error, erp_views::ViewError::InvalidDescription(_)),
        "structural failure was misclassified: {error}"
    );
    assert!(error.to_string().contains("earlier construction failure"));
}

#[test]
fn sticky_failure_also_refuses_serializing_an_earlier_constructed_payload() {
    let schema = ViewSchema::new(1, &TEXT).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|value| value.text("already built"))
        .unwrap();
    let failure = DescriptionBuilder::new(&schema, &budget)
        .build(|value| value.boolean(true))
        .unwrap_err();
    assert!(matches!(
        failure,
        erp_views::ViewError::InvalidDescription(_)
    ));
    let error = serde_json::to_vec(&payload).unwrap_err();
    assert!(error.to_string().contains("earlier construction failure"));
}

#[test]
fn constructed_handles_keep_explicit_model_scope_for_later_authorization() {
    static HANDLE: SchemaNode = SchemaNode::FieldHandle;
    let schema = ViewSchema::new(1, &HANDLE).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|value| value.field_handle("res.users", "password"))
        .unwrap();
    let visitor = ReferenceVisitor::new(schema);
    let refs = visitor.payload_references(&payload, &budget).unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![erp_views::FieldReference::ModelPath {
            model: "res.users",
            path: "password"
        }]
    );
    let encoded = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT)
        .encode(&payload)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(encoded.as_json_bytes()).unwrap(),
        json!({"model":"res.users", "name":"password"})
    );
}

// ---- WP2: group_by and variant constructors ---------------------------------------------------

static GROUP: SchemaNode = SchemaNode::GroupBy {
    scope: ReferenceScope::Root,
};
static BY_FIELD: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD),
    SchemaProperty::optional("label", &TEXT),
]);
static BY_GROUP: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("group", &GROUP)]);
static UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("field", &BY_FIELD),
        SchemaVariant::new("group", &BY_GROUP),
    ],
};
static UNIONS: SchemaNode = SchemaNode::Array {
    item: &UNION,
    max_items: 8,
};
fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}

#[test]
fn group_by_constructor_keeps_the_text_and_emits_the_path_without_the_suffix() {
    let schema = ViewSchema::new(1, &GROUP).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.group_by("create_date:month"))
        .unwrap();
    let refs = ReferenceVisitor::new(schema)
        .payload_references(&payload, &budget)
        .unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![FieldReference::RootPath("create_date")]
    );
    let bytes = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT)
        .encode(&payload)
        .unwrap();
    assert_eq!(bytes.as_json_bytes(), br#""create_date:month""#);
}

#[test]
fn group_by_constructor_refuses_bad_text_and_wrong_slots_stickily() {
    let schema = ViewSchema::new(1, &GROUP).unwrap();
    let budget = DescriptionBudget::new();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| root.group_by("create_date:decade"))
        .unwrap_err();
    assert_eq!(
        error,
        invalid(
            "Description group_by granularity 'decade' is unsupported; expected day, week, month, quarter or year"
        )
    );
    assert_eq!(
        budget.check().unwrap_err(),
        invalid("Description remains refused after an earlier construction failure")
    );
    let text = ViewSchema::new(1, &TEXT).unwrap();
    let error = DescriptionBuilder::new(&text, &DescriptionBudget::new())
        .build(|root| root.group_by("name"))
        .unwrap_err();
    assert_eq!(
        error,
        invalid("Description value does not match its group_by schema slot")
    );
}

/// independent walk of the constructed payload)
#[test]
fn variant_constructor_serializes_the_tag_first_and_walks_only_its_branch() {
    let schema = ViewSchema::new(1, &UNIONS).unwrap();
    let budget = DescriptionBudget::new();
    let payload = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            root.array(2, |index, item| {
                item.variant(if index == 0 { "field" } else { "group" }, |branch| {
                    branch.object(|object| {
                        if index == 0 {
                            object.set("field", |v| v.field_path("partner_id.email"))
                        } else {
                            object.set("group", |v| v.group_by("create_date:week"))
                        }
                    })
                })
            })
        })
        .unwrap();
    let refs = ReferenceVisitor::new(schema)
        .payload_references(&payload, &budget)
        .unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![
            FieldReference::RootPath("create_date"),
            FieldReference::RootPath("partner_id.email")
        ]
    );
    let bytes = DescriptionEncoder::new(&budget, &ViewLimits::DEFAULT)
        .encode(&payload)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(bytes.as_json_bytes()).unwrap(),
        json!([
            {"kind":"field", "field":"partner_id.email"},
            {"kind":"group", "group":"create_date:week"}
        ])
    );
    assert!(
        std::str::from_utf8(bytes.as_json_bytes())
            .unwrap()
            .starts_with(r#"[{"kind":"field","#),
        "the discriminator is written first"
    );
}

#[test]
fn variant_constructor_refuses_unknown_tags_members_and_foreign_values() {
    let schema = ViewSchema::new(1, &UNION).unwrap();
    let build = |build: &dyn Fn(
        erp_views::DescriptionValueBuilder<'_>,
    ) -> Result<erp_views::DescriptionValue, ViewError>| {
        let budget = DescriptionBudget::new();
        let result = DescriptionBuilder::new(&schema, &budget).build(build);
        (budget, result)
    };
    let (budget, result) = build(&|root| root.variant("other", |b| b.object(|_| Ok(()))));
    assert_eq!(
        result.unwrap_err(),
        invalid("Description tagged union has an undeclared variant")
    );
    assert!(budget.check().is_err(), "sticky");
    // Missing required member.
    let (_, result) = build(&|root| root.variant("field", |b| b.object(|_| Ok(()))));
    assert_eq!(
        result.unwrap_err(),
        invalid("Description payload is missing a required property")
    );
    // Duplicate member.
    let (_, result) = build(&|root| {
        root.variant("field", |b| {
            b.object(|o| {
                o.set("field", |v| v.field_path("name"))?;
                o.set("field", |v| v.field_path("other"))
            })
        })
    });
    assert_eq!(
        result.unwrap_err(),
        invalid("Description payload has a duplicate property")
    );
    // The discriminator is not a member the branch can set.
    let (_, result) = build(&|root| {
        root.variant("field", |b| {
            b.object(|o| {
                o.set("field", |v| v.field_path("name"))?;
                o.set("kind", |v| v.text("field"))
            })
        })
    });
    assert_eq!(
        result.unwrap_err(),
        invalid("Description payload has an undeclared property")
    );
    // A branch cannot return a value that was built for another slot.
    let (_, result) =
        build(&|root| root.variant("field", |_| root.variant("group", |b| b.object(|_| Ok(())))));
    assert!(result.is_err());
    // Not a union slot.
    let text = ViewSchema::new(1, &TEXT).unwrap();
    let error = DescriptionBuilder::new(&text, &DescriptionBudget::new())
        .build(|root| root.variant("field", |b| b.object(|_| Ok(()))))
        .unwrap_err();
    assert_eq!(
        error,
        invalid("Description value does not match its tagged union schema slot")
    );
}

#[test]
fn variant_storage_is_charged_before_the_branch_is_built() {
    let schema = ViewSchema::new(1, &UNION).unwrap();
    let budget = DescriptionBudget::with_limit(1).unwrap();
    let mut called = false;
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            root.variant("field", |b| {
                called = true;
                b.object(|_| Ok(()))
            })
        })
        .unwrap_err();
    assert!(
        !called,
        "the branch builder must not run once admission failed"
    );
    assert!(matches!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: 1,
            actual: Some(_)
        }
    ));
}

#[test]
fn callback_cursor_budget_shares_the_request_construction_allowance() {
    let schema = ViewSchema::new(1, &TEXT).unwrap();
    let budget = DescriptionBudget::with_limit(16).unwrap();
    let error = DescriptionBuilder::new(&schema, &budget)
        .build(|root| {
            let _held = root.budget().reserve(16)?;
            root.text("x")
        })
        .unwrap_err();
    assert_eq!(
        error,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: 16,
            actual: Some(17),
        }
    );
    assert_eq!(
        error.to_string(),
        "simultaneously retained description bytes limit exceeded: maximum 16, reached 17"
    );
    let sticky = budget.check().unwrap_err();
    assert_eq!(
        sticky,
        ViewError::LimitExceeded {
            limit: LimitKind::DescriptionBuildBytes,
            max: 16,
            actual: None,
        }
    );
    assert_eq!(
        sticky.to_string(),
        "simultaneously retained description bytes limit exceeded: maximum 16"
    );
}
