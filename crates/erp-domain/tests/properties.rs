//! Property tests over generated JSON domains: parsing and optimizing never panic, optimization is
//! idempotent and stable through a JSON round trip, and `Hash` agrees with `Eq`.

// Test crate: helpers outside `#[test]` functions may unwrap and panic too (clippy.toml intent).
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use common::model;
use erp_domain::{Domain, Operator};
use proptest::prelude::*;
use proptest::sample::select;
use serde_json::{Value as Json, json};

const MIXED: &str = "test_orm.mixed";

fn hash(domain: &Domain) -> u64 {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

fn field() -> impl Strategy<Value = &'static str> {
    select(vec![
        "id",
        "integer",
        "numeric",
        "char",
        "html",
        "boolean",
        "date",
        "moment",
        "selection",
        "currency_id",
        "currency_id.name",
        "many2one_id",
        "many2one_id.name",
        "one2many_ids",
        "many2many_ids",
        "many2many_ids.name",
        "nope",
    ])
}

fn operator() -> impl Strategy<Value = String> {
    let mut ops: Vec<String> = Operator::ALL
        .iter()
        .map(|op| op.as_str().to_owned())
        .collect();
    ops.push("bogus".to_owned());
    select(ops)
}

fn scalar() -> impl Strategy<Value = Json> {
    prop_oneof![
        any::<i64>().prop_map(|i| json!(i)),
        (-1.0e6..1.0e6f64).prop_map(|f| json!(f)),
        any::<bool>().prop_map(|b| json!(b)),
        Just(Json::Null),
        select(vec![
            "",
            "x",
            "abc",
            "2024-01-05",
            "2024-01-05 10:00:00",
            "2024-13-01",
            "today",
            "+1d",
            "now -2H",
            "hello",
            "true",
        ])
        .prop_map(|s| json!(s)),
    ]
}

fn value() -> impl Strategy<Value = Json> {
    prop_oneof![
        3 => scalar(),
        1 => prop::collection::vec(scalar(), 0..4).prop_map(Json::Array),
    ]
}

fn leaf() -> impl Strategy<Value = Vec<Json>> {
    (field(), operator(), value()).prop_map(|(f, op, v)| vec![json!([f, op, v])])
}

/// Prefix-list domains: conditions, `!`, n-ary `&`/`|`, and `any` sub-domains.
fn tree() -> impl Strategy<Value = Vec<Json>> {
    leaf().prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            inner.clone().prop_map(|child| {
                let mut items = vec![json!("!")];
                items.extend(child);
                items
            }),
            (
                select(vec!["&", "|"]),
                prop::collection::vec(inner.clone(), 2..4)
            )
                .prop_map(|(op, children)| {
                    let mut items = vec![json!(op); children.len() - 1];
                    for child in children {
                        items.extend(child);
                    }
                    items
                }),
            (
                select(vec![
                    "currency_id",
                    "many2one_id",
                    "one2many_ids",
                    "integer"
                ]),
                select(vec!["any", "not any"]),
                inner,
            )
                .prop_map(|(f, op, sub)| vec![json!([f, op, sub])]),
        ]
    })
}

/// Implicit AND of a few trees.
fn domain_json() -> impl Strategy<Value = Json> {
    prop::collection::vec(tree(), 1..3).prop_map(|trees| Json::Array(trees.concat()))
}

/// Arbitrary token soup, mostly malformed.
fn tokens() -> impl Strategy<Value = Json> {
    prop::collection::vec(
        prop_oneof![
            leaf().prop_map(|mut l| l.remove(0)),
            Just(json!("&")),
            Just(json!("|")),
            Just(json!("!")),
            Just(json!(42)),
            Just(json!([1, "=", 1])),
            Just(json!(["a", "b"])),
        ],
        0..12,
    )
    .prop_map(Json::Array)
}

proptest! {
    /// Garbage in: an error, never a panic.
    #[test]
    fn token_soup_never_panics(input in tokens()) {
        if let Ok(domain) = Domain::from_json(&input) {
            let _ = domain.optimize(&model(MIXED));
        }
    }

    /// Structural properties of parsed and optimized domains.
    #[test]
    fn parse_and_optimize_properties(input in domain_json()) {
        let Ok(domain) = Domain::from_json(&input) else {
            return Ok(());
        };
        // Parsing is deterministic and Hash agrees with Eq.
        let again = Domain::from_json(&input).unwrap();
        prop_assert_eq!(&again, &domain);
        prop_assert_eq!(hash(&again), hash(&domain));
        // List syntax round trip.
        let rendered = domain.to_json().unwrap();
        let reparsed = Domain::from_json(&rendered).unwrap();
        prop_assert_eq!(&reparsed, &domain);
        prop_assert_eq!(hash(&reparsed), hash(&domain));
        // Double negation at construction is the identity.
        prop_assert_eq!(&!!domain.clone(), &domain);

        let ctx = model(MIXED);
        let Ok(optimized) = domain.optimize(&ctx) else {
            return Ok(());
        };
        // Idempotent, also without the cached level: re-optimizing the rendered form gives the
        // same domain.
        prop_assert_eq!(&optimized.optimize(&ctx).unwrap(), &optimized);
        if let Ok(rendered) = optimized.to_json() {
            let fresh = Domain::from_json(&rendered).unwrap();
            let reoptimized = fresh.optimize(&ctx).unwrap();
            prop_assert_eq!(&reoptimized, &optimized, "rendered: {}", rendered);
            prop_assert_eq!(hash(&reoptimized), hash(&optimized));
        }
        // Negating the optimized form and optimizing never fails.
        prop_assert!((!&optimized).optimize(&ctx).is_ok());
    }
}
