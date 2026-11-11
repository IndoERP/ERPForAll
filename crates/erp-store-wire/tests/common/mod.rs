//! Shared fixtures for the integration tests.
#![allow(dead_code)]

use erp_store_wire::{
    BusCursor, FieldKind, FieldSpec, SchemaGeneration, SnapshotDescriptor, StoreSchema, Xid8,
};

pub fn x(value: u64) -> Xid8 {
    Xid8::new(value).expect("nonzero xid")
}

/// Read-only descriptor `100:104:100,103` with no own xid.
pub fn read_only() -> SnapshotDescriptor {
    SnapshotDescriptor::new(x(100), x(104), vec![x(100), x(103)], None).expect("valid")
}

/// The same snapshot taken by a writing transaction with xid 104 (A1: own >= xmax).
pub fn writing() -> SnapshotDescriptor {
    SnapshotDescriptor::new(x(100), x(104), vec![x(100), x(103)], Some(x(104))).expect("valid")
}

pub fn cursor_104() -> BusCursor {
    BusCursor::new(x(104), 5512).expect("valid cursor")
}

/// A generic three-model schema covering every kind and sync class.
pub fn schema() -> StoreSchema {
    let mut b = StoreSchema::builder(SchemaGeneration::new("g1").expect("token"));
    let f = |b: &mut erp_store_wire::SchemaBuilder, m: &str, name: &str, spec: FieldSpec| {
        b.field(m, name, spec).expect("valid field");
    };
    for (name, kind) in [
        ("active", FieldKind::Bool),
        ("seq", FieldKind::Int),
        ("ratio", FieldKind::Float),
        ("title", FieldKind::String),
        ("body", FieldKind::Html),
        ("day", FieldKind::Date),
        ("stamp", FieldKind::Datetime),
        ("state", FieldKind::Selection),
        ("owner", FieldKind::Reference),
    ] {
        f(&mut b, "test.thread", name, FieldSpec::new(kind));
    }
    f(
        &mut b,
        "test.thread",
        "author",
        FieldSpec::new(FieldKind::Many2One).target("test.partner"),
    );
    f(
        &mut b,
        "test.thread",
        "messages",
        FieldSpec::new(FieldKind::X2Many).target("test.message"),
    );
    f(
        &mut b,
        "test.thread",
        "unread",
        FieldSpec::new(FieldKind::Counter),
    );
    f(
        &mut b,
        "test.thread",
        "stats",
        FieldSpec::new(FieldKind::String).answer_only(),
    );
    f(
        &mut b,
        "test.message",
        "text",
        FieldSpec::new(FieldKind::String),
    );
    f(
        &mut b,
        "test.message",
        "thread",
        FieldSpec::new(FieldKind::Many2One).target("test.thread"),
    );
    f(
        &mut b,
        "test.partner",
        "name",
        FieldSpec::new(FieldKind::String),
    );
    f(
        &mut b,
        "test.partner",
        "score",
        FieldSpec::new(FieldKind::Counter).cap(99),
    );
    b.build().expect("valid schema")
}

/// Assert kind code, path and the whole message of a refusal.
pub fn assert_err<T: std::fmt::Debug>(
    actual: Result<T, erp_store_wire::WireError>,
    code: &str,
    path: Option<&str>,
    message: &str,
) {
    let err = actual.expect_err("must be refused");
    assert_eq!(err.code(), code, "{err}");
    assert_eq!(err.path(), path, "{err}");
    assert_eq!(err.to_string(), message);
}
