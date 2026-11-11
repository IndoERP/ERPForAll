//! Payload caps (50,000 nodes) are separate from the registration caps (4,096 nodes, depth 64,
//! 256 KiB): a schema stays small while one payload may legitimately carry far more values than
//! the schema has slots (WP2). Uses only the WP1 API so the
//! failure before the change is behavioural, not a missing symbol.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_views::{
    DescriptionBudget, DescriptionBuilder, ReferenceVisitor, SchemaNode, SchemaProperty, ViewError,
    ViewSchema,
};
use serde_json::Value;

static BOOL: SchemaNode = SchemaNode::Bool;
static INNER: SchemaNode = SchemaNode::Array {
    item: &BOOL,
    max_items: 4096,
};
static OUTER: SchemaNode = SchemaNode::Array {
    item: &INNER,
    max_items: 16,
};

/// Thirteen inner arrays: `1 + 13 + bools` payload nodes.
fn bool_rows(bools: usize) -> Vec<usize> {
    let mut rows = vec![4096; 12];
    rows.push(bools - 4096 * 12);
    rows
}
fn raw_payload(bools: usize) -> Value {
    Value::Array(
        bool_rows(bools)
            .into_iter()
            .map(|n| Value::Array(vec![Value::Bool(true); n]))
            .collect(),
    )
}

/// whole multi-view response is the producer's, WP5)
#[test]
fn raw_payload_walk_accepts_fifty_thousand_nodes_and_refuses_one_beyond() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &OUTER).unwrap());
    // 1 outer + 13 inner + 49,986 booleans = 50,000 nodes exactly.
    let exact = raw_payload(49_986);
    visitor
        .references(&exact, &DescriptionBudget::new())
        .unwrap();
    let beyond = raw_payload(49_987);
    let error = visitor
        .references(&beyond, &DescriptionBudget::new())
        .unwrap_err();
    assert_eq!(
        error,
        ViewError::InvalidDescription("Description payload exceeds its node limit".to_owned())
    );
}

#[test]
fn constructed_payload_walk_accepts_fifty_thousand_nodes_and_refuses_one_beyond() {
    let schema = ViewSchema::new(1, &OUTER).unwrap();
    let build = |bools: usize| {
        let budget = DescriptionBudget::new();
        let rows = bool_rows(bools);
        let result = DescriptionBuilder::new(&schema, &budget).build(|root| {
            root.array(rows.len(), |row, inner| {
                inner.array(rows[row], |_, flag| flag.boolean(true))
            })
        });
        (budget, result)
    };
    let (budget, payload) = build(49_986);
    let payload = payload.unwrap();
    ReferenceVisitor::new(schema.clone())
        .payload_references(&payload, &budget)
        .unwrap();
    let (_budget, beyond) = build(49_987);
    assert_eq!(
        beyond.unwrap_err(),
        ViewError::InvalidDescription("Description payload exceeds its node limit".to_owned())
    );
}

fn chain(arrays: usize) -> &'static SchemaNode {
    let mut node: &'static SchemaNode = &BOOL;
    for _ in 0..arrays {
        node = Box::leak(Box::new(SchemaNode::Array {
            item: node,
            max_items: 1,
        }));
    }
    node
}

#[test]
fn registration_depth_cap_is_sixty_four_and_one_beyond_refuses() {
    ViewSchema::new(1, chain(64)).unwrap();
    let error = ViewSchema::new(1, chain(65)).unwrap_err();
    assert_eq!(
        error,
        ViewError::InvalidDescription(
            "Description schema exceeds its node, depth or byte limit".to_owned()
        )
    );
}

#[test]
fn registration_caps_refuse_a_schema_with_too_many_properties() {
    let properties: Vec<SchemaProperty> = (0..5_000)
        .map(|index| {
            let name: &'static str = Box::leak(format!("p{index}").into_boxed_str());
            SchemaProperty::optional(name, &BOOL)
        })
        .collect();
    let object = Box::leak(Box::new(SchemaNode::Object(Box::leak(
        properties.into_boxed_slice(),
    ))));
    let error = ViewSchema::new(1, object).unwrap_err();
    assert_eq!(
        error,
        ViewError::InvalidDescription("Description schema has too many properties".to_owned())
    );
}
