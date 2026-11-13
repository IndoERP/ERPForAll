// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded typed reader input; database and RPC coverage are separate.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::ErrorKind;
use erp_orm::audit::reader::{AuditQuery, EntryKey, ReadRequest};
use serde_json::{Value, json};

fn query() -> Value {
    json!({"model":"res.partner", "from":"2026-11-01T00:00:00Z",
        "until":"2026-11-02T00:00:00Z", "fields":["entry_id","target_ids"]})
}
fn invalid(value: Value, message: &str) {
    let e = AuditQuery::parse(&value).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), message);
}
#[test]
fn audit_reader_query_is_closed_and_bounded() {
    assert_eq!(AuditQuery::parse(&query()).unwrap().page_size(), 100);
    for field in [
        "args_digest",
        "events",
        "actor_id",
        "company_ids",
        "id",
        "unknown",
    ] {
        let mut q = query();
        q["fields"] = json!([field]);
        invalid(q, "a later release: unsupported projection field");
    }
    for key in ["domain", "order", "group_by", "count", "sql"] {
        let mut q = query();
        q[key] = json!("anything");
        invalid(q, "a later release: unknown query key");
    }
    for value in [
        json!(0),
        json!(1001),
        json!(-1),
        json!(1.5),
        json!("100"),
        Value::Null,
    ] {
        let mut q = query();
        q["limit"] = value;
        invalid(q, "a later release: limit must be an integer in 1..=1000");
    }
    let mut q = query();
    q["until"] = json!("2026-12-03T00:00:00Z");
    invalid(
        q,
        "a later release: interval must be positive and at most 31 days",
    );
    let mut q = query();
    q["from"] = q["until"].clone();
    invalid(
        q,
        "a later release: interval must be positive and at most 31 days",
    );
    let mut q = query();
    q["fields"] = json!([]);
    invalid(
        q,
        "a later release: fields must be a nonempty array of at most 8 names",
    );
    let mut q = query();
    q["continuation"] = json!("x".repeat(1025));
    invalid(q, "a later release: invalid continuation");
}
#[test]
fn audit_reader_keys_are_exact_and_direct_reads_deduplicate() {
    let key = json!({"entry_id":"018f3a1c-7b2d-7c4e-9a10-00000000aa01", "create_date":"2026-11-01T00:00:00.000001Z"});
    let parsed = EntryKey::parse(&key).unwrap();
    assert_eq!(parsed.to_json(), key);
    let request =
        ReadRequest::parse(&json!({"keys":[key.clone(),key.clone()],"fields":["entry_id"]}))
            .unwrap();
    assert_eq!(request.len(), 1);
    let e = ReadRequest::parse(&json!({"keys":vec![key;1001],"fields":["entry_id"]})).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "a later release: keys must be an array of at most 1000 composite keys"
    );
    let e = EntryKey::parse(&json!({"entry_id":"bad","create_date":"2026-11-01T00:00:00Z"}))
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), "a later release: invalid composite entry key");
}
