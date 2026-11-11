//! Schema-derived visitors cannot accept an unchecked or undeclared payload property.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_views::{
    DescriptionBudget, FieldReference, LimitKind, ReferenceScope, ReferenceVisitor, SchemaNode,
    SchemaProperty, SchemaVariant, ViewError, ViewSchema,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

static FIELD: SchemaNode = SchemaNode::FieldPath;
static NAME: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static NESTED: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("field", &FIELD)]);
static ARRAY: SchemaNode = SchemaNode::Array {
    item: &NESTED,
    max_items: 16,
};
static ROOT: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("label", &NAME),
    SchemaProperty::required("nodes", &ARRAY),
]);

#[test]
fn schema_walk_includes_every_nested_field_and_rejects_undeclared_data() {
    let schema = ViewSchema::new(1, &ROOT).unwrap();
    let visitor = ReferenceVisitor::new(schema);
    let budget = DescriptionBudget::new();
    let payload = json!({"label":"Allowed", "nodes":[{"field":"company_id.partner_id.email"}]});
    let refs = visitor.references(&payload, &budget).unwrap();
    assert_eq!(
        refs.fields().collect::<BTreeSet<_>>(),
        BTreeSet::from([erp_views::FieldReference::RootPath(
            "company_id.partner_id.email"
        )])
    );
    assert!(visitor.references(&json!({"label":"Allowed", "nodes":[{"field":"name", "omitted_secret":"internal_memo"}]}), &budget).is_err());
    assert!(
        visitor
            .references(&json!({"label":"Allowed", "nodes":[{}]}), &budget)
            .is_err()
    );
    assert!(
        visitor
            .references(
                &json!({"label":"Allowed", "nodes":[{"field":"company_id..email"}]}),
                &budget
            )
            .is_err()
    );
}

#[test]
fn schema_construction_rejects_duplicates_and_unbounded_arrays() {
    static DUP: SchemaNode = SchemaNode::Object(&[
        SchemaProperty::required("same", &NAME),
        SchemaProperty::required("same", &NAME),
    ]);
    static UNBOUNDED: SchemaNode = SchemaNode::Array {
        item: &NAME,
        max_items: usize::MAX,
    };
    assert!(ViewSchema::new(1, &DUP).is_err());
    assert!(ViewSchema::new(1, &UNBOUNDED).is_err());
    assert!(ViewSchema::new(0, &ROOT).is_err());
}

#[test]
fn field_slots_accept_the_registry_identifier_contract() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &FIELD).unwrap());
    let payload = json!("company_id._Custom$1");
    let refs = visitor
        .references(&payload, &DescriptionBudget::new())
        .unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![erp_views::FieldReference::RootPath("company_id._Custom$1")]
    );
}

#[test]
fn explicit_model_field_handles_never_become_root_relative_paths() {
    static HANDLE: SchemaNode = SchemaNode::FieldHandle;
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &HANDLE).unwrap());
    let payload = json!({"model":"res.users", "name":"password"});
    let refs = visitor
        .references(&payload, &DescriptionBudget::new())
        .unwrap();
    assert_eq!(
        refs.fields().collect::<Vec<_>>(),
        vec![erp_views::FieldReference::ModelPath {
            model: "res.users",
            path: "password"
        }]
    );
    assert!(
        visitor
            .references(
                &json!({"model":"res.users", "name":"password", "mask":true}),
                &DescriptionBudget::new()
            )
            .is_err()
    );
    assert!(
        visitor
            .references(
                &json!({"model":"res.users", "name":null}),
                &DescriptionBudget::new()
            )
            .is_err()
    );
}

// ---- WP2: group-by and tagged-union slots -------------------------------------------------

static ROOT_GROUP: SchemaNode = SchemaNode::GroupBy {
    scope: ReferenceScope::Root,
};
static MODEL_GROUP: SchemaNode = SchemaNode::GroupBy {
    scope: ReferenceScope::Model("sale.order"),
};

fn invalid(message: &str) -> ViewError {
    ViewError::InvalidDescription(message.to_owned())
}
fn describe(references: &erp_views::SchemaReferences<'_>) -> Vec<String> {
    references
        .fields()
        .map(|field| match field {
            FieldReference::RootPath(path) => format!("root:{path}"),
            FieldReference::ModelPath { model, path } => format!("{model}:{path}"),
            _ => panic!("unexpected reference kind"),
        })
        .collect()
}
fn raw(node: &'static SchemaNode, payload: &Value) -> Result<Vec<String>, ViewError> {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, node).unwrap());
    let budget = DescriptionBudget::new();
    let references = visitor.references(payload, &budget)?;
    Ok(describe(&references))
}

#[test]
fn group_by_slot_emits_the_path_without_its_granularity_suffix() {
    assert_eq!(
        raw(&ROOT_GROUP, &json!("partner_id")).unwrap(),
        ["root:partner_id"]
    );
    for granularity in ["day", "week", "month", "quarter", "year"] {
        assert_eq!(
            raw(&ROOT_GROUP, &json!(format!("create_date:{granularity}"))).unwrap(),
            ["root:create_date"],
            "{granularity}"
        );
    }
    assert_eq!(
        raw(&ROOT_GROUP, &json!("company_id.partner_id:year")).unwrap(),
        ["root:company_id.partner_id"]
    );
    assert_eq!(
        raw(&MODEL_GROUP, &json!("partner_id:month")).unwrap(),
        ["sale.order:partner_id"]
    );
}

#[test]
fn group_by_slot_refuses_unsupported_granularity_and_malformed_paths() {
    let granularity = |name: &str| {
        invalid(&format!(
            "Description group_by granularity '{name}' is unsupported; expected day, week, month, quarter or year"
        ))
    };
    assert_eq!(
        raw(&ROOT_GROUP, &json!("create_date:fortnight")).unwrap_err(),
        granularity("fortnight")
    );
    assert_eq!(
        raw(&ROOT_GROUP, &json!("create_date:")).unwrap_err(),
        granularity("")
    );
    assert_eq!(
        raw(&ROOT_GROUP, &json!("create_date:month:day")).unwrap_err(),
        invalid("Description group_by has more than one granularity suffix")
    );
    for path in [":month", "", "a..b", "a b:day"] {
        assert_eq!(
            raw(&ROOT_GROUP, &json!(path)).unwrap_err(),
            invalid("Description group_by has an invalid field path"),
            "{path:?}"
        );
    }
    assert_eq!(
        raw(&ROOT_GROUP, &json!(5)).unwrap_err(),
        invalid("Description group_by requires text")
    );
}

static TEXT64: SchemaNode = SchemaNode::Text { max_bytes: 64 };
static BY_FIELD: SchemaNode = SchemaNode::Object(&[
    SchemaProperty::required("field", &FIELD),
    SchemaProperty::optional("label", &TEXT64),
]);
static BY_GROUP: SchemaNode = SchemaNode::Object(&[SchemaProperty::required("group", &ROOT_GROUP)]);
static UNION: SchemaNode = SchemaNode::TaggedUnion {
    discriminator: "kind",
    variants: &[
        SchemaVariant::new("field", &BY_FIELD),
        SchemaVariant::new("group", &BY_GROUP),
    ],
};

/// slots arrive with WP3+)
#[test]
fn tagged_union_visits_exactly_the_selected_branch() {
    assert_eq!(
        raw(
            &UNION,
            &json!({"kind":"field", "field":"partner_id.email", "label":"x"})
        )
        .unwrap(),
        ["root:partner_id.email"]
    );
    assert_eq!(
        raw(&UNION, &json!({"kind":"group", "group":"create_date:week"})).unwrap(),
        ["root:create_date"]
    );
}

#[test]
fn tagged_union_refuses_unknown_tags_discriminators_and_wrong_shapes() {
    let cases: [(Value, &str); 8] = [
        (
            json!({"kind":"other"}),
            "Description tagged union has an undeclared variant",
        ),
        (
            json!({"field":"name"}),
            "Description tagged union requires a text discriminator",
        ),
        (
            json!({"kind":5, "field":"name"}),
            "Description tagged union requires a text discriminator",
        ),
        (
            json!("field"),
            "Description tagged union requires an object",
        ),
        (
            json!({"kind":"field", "field":"name", "secret":"internal_memo"}),
            "Description payload has an undeclared property",
        ),
        // `field` belongs to the other branch.
        (
            json!({"kind":"group", "field":"name"}),
            "Description payload has an undeclared property",
        ),
        (
            json!({"kind":"field"}),
            "Description payload is missing a required property",
        ),
        (
            json!({"kind":"field", "field":7}),
            "Description field reference requires text",
        ),
    ];
    for (payload, message) in cases {
        assert_eq!(
            raw(&UNION, &payload).unwrap_err(),
            invalid(message),
            "{payload}"
        );
    }
}

#[test]
fn declarations_of_the_new_slots_are_validated_when_the_schema_is_admitted() {
    static NOTHING: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "kind",
        variants: &[],
    };
    static DUPLICATE: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "kind",
        variants: &[
            SchemaVariant::new("field", &BY_FIELD),
            SchemaVariant::new("field", &BY_GROUP),
        ],
    };
    static BAD_TAG: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "kind",
        variants: &[SchemaVariant::new("Field", &BY_FIELD)],
    };
    static BAD_DISCRIMINATOR: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "Kind",
        variants: &[SchemaVariant::new("field", &BY_FIELD)],
    };
    static NOT_OBJECT: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "kind",
        variants: &[SchemaVariant::new("field", &TEXT64)],
    };
    static COLLIDES: SchemaNode = SchemaNode::TaggedUnion {
        discriminator: "field",
        variants: &[SchemaVariant::new("field", &BY_FIELD)],
    };
    static EMPTY_MODEL: SchemaNode = SchemaNode::GroupBy {
        scope: ReferenceScope::Model(""),
    };
    static EMPTY_PARENT: SchemaNode = SchemaNode::Expression {
        kind: erp_views::ExpressionKind::Value,
        scope: ReferenceScope::Root,
        parent_model: Some(""),
    };
    let cases: [(&'static SchemaNode, &str); 8] = [
        (
            &NOTHING,
            "Description schema tagged union requires at least one variant",
        ),
        (
            &DUPLICATE,
            "Description schema tagged union has an invalid or duplicate variant tag",
        ),
        (
            &BAD_TAG,
            "Description schema tagged union has an invalid or duplicate variant tag",
        ),
        (
            &BAD_DISCRIMINATOR,
            "Description schema tagged union has an invalid discriminator",
        ),
        (
            &NOT_OBJECT,
            "Description schema tagged union variant must be an object",
        ),
        (
            &COLLIDES,
            "Description schema variant property collides with its discriminator",
        ),
        (
            &EMPTY_MODEL,
            "Description schema has an invalid model scope",
        ),
        (
            &EMPTY_PARENT,
            "Description schema has an invalid model scope",
        ),
    ];
    for (node, message) in cases {
        assert_eq!(
            ViewSchema::new(1, node).unwrap_err(),
            invalid(message),
            "{message}"
        );
    }
    ViewSchema::new(1, &UNION).unwrap();
}

/// its storage exists)
#[test]
fn derived_reference_storage_is_charged_to_the_shared_budget() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &ROOT_GROUP).unwrap());
    let budget = DescriptionBudget::with_limit(1).unwrap();
    let error = visitor
        .references(&json!("create_date:month"), &budget)
        .unwrap_err();
    assert!(
        matches!(
            error,
            ViewError::LimitExceeded {
                limit: LimitKind::DescriptionBuildBytes,
                max: 1,
                actual: Some(_)
            }
        ),
        "{error:?}"
    );
}
